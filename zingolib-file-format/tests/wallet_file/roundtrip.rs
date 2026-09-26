//! Property tests over the wallet file: write, read, write again must reproduce the first
//! bytes exactly, plain and encrypted, and `read` must refuse the bytes for any other chain.
//!
//! One property varies the parts that have no chain dependency: unified and transparent
//! address ids, wallet settings, the price list, sync state and the outpoint map. The other
//! varies the scanned data: wallet blocks, transactions with notes in every pool, outgoing
//! notes, coins, spends, nullifiers, and shard trees past the checkpoint cap.
//!
//! `SyncState`, `PriceList`, `WalletBlock` and the outgoing notes have no public constructor that
//! takes arbitrary field values, so they are built the same way a real reader builds them:
//! encode the fields as the documented wire format and decode with the type's own public
//! `read`.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::num::NonZeroU32;
use std::ops::Range;

use byteorder::{LittleEndian, WriteBytesExt};
use incrementalmerkletree::{Marking, Position, Retention};
use orchard::note::{NoteVersion, RandomSeed, Rho};
use orchard::tree::MerkleHashOrchard;
use orchard::value::NoteValue;
use proptest::prelude::*;

use zcash_encoding::{Optional, Vector};
use zcash_primitives::transaction::TxId;
use zcash_protocol::consensus::BlockHeight;
use zcash_protocol::memo::Memo;
use zcash_protocol::value::Zatoshis;
use zcash_transparent::address::TransparentAddress;
use zcash_transparent::keys::NonHardenedChildIndex;
use zip32::{AccountId, Scope};

use pepper_sync::config::{
    PerformanceLevel, SyncConfig, TransparentAddressDiscovery, TransparentAddressDiscoveryScopes,
};
use pepper_sync::keys::KeyId;
use pepper_sync::keys::transparent::{TransparentAddressId, encode_address};
use pepper_sync::sync::ScanPriority;
use pepper_sync::wallet::{
    IronwoodNote, OrchardNote, OutgoingIronwoodNote, OutgoingOrchardNote, OutgoingSaplingNote,
    OutputId, OutputInterface, SaplingNote, ScanTarget, ShardTrees, SyncState, TransparentCoin,
    WalletBlock, WalletNote, WalletTransaction,
};
use zingo_common_components::protocol::ActivationHeights;
use zingolib_common::chain::ChainType;
use zingolib_common::keys::{
    ReceiverSelection, TransparentScope, UnifiedAddressId, UnifiedKeyStore,
};
use zingolib_common::serialization::ReadableWriteable;
use zingolib_common::status::ConfirmationStatus;
use zingolib_file_format::{WalletFile, WalletSettings};
use zingolib_price::{Price, PriceList};

use super::support::{bytes, fast_session, fresh};

const PASSPHRASE: &str = "conformance";

/// The reader bounds a transparent coin to the money supply; note values follow the same bound
/// so the wallets stay realistic.
const MAX_MONEY: u64 = 21_000_000 * 100_000_000;

/// Above pepper-sync's `MAX_REORG_ALLOWANCE` of 100, so a tree with a checkpoint on every leaf
/// exercises the writer's checkpoint pruning.
const MAX_TREE_LEAVES: usize = 130;

/// Fixed mnemonic seed for the chain-dependent parts of the wallet (keys, address, mnemonic).
/// The property under test never varies these, only the fields listed in the module doc.
const BASE_SEED: u8 = 1;

/// Every scan priority tag, in the order [`SyncState::read`] decodes them.
const SCAN_PRIORITIES: [ScanPriority; 9] = [
    ScanPriority::RefetchingNullifiers,
    ScanPriority::Scanning,
    ScanPriority::Scanned,
    ScanPriority::ScannedWithoutMapping,
    ScanPriority::Historic,
    ScanPriority::OpenAdjacent,
    ScanPriority::FoundNote,
    ScanPriority::ChainTip,
    ScanPriority::Verify,
];

fn scan_priority_tag(priority: ScanPriority) -> u8 {
    SCAN_PRIORITIES
        .iter()
        .position(|&candidate| candidate == priority)
        .expect("every ScanPriority is listed in SCAN_PRIORITIES") as u8
}

fn chain_type_strategy() -> impl Strategy<Value = ChainType> {
    prop_oneof![
        Just(ChainType::Mainnet),
        Just(ChainType::Testnet),
        Just(ChainType::Regtest(ActivationHeights::default())),
    ]
}

/// A chain distinct from `chain_type`, by wallet-file tag. Any two distinct chain types have
/// different `Display` strings, which is all `WalletFile::read` checks.
fn a_different_chain(chain_type: ChainType) -> ChainType {
    match chain_type {
        ChainType::Mainnet => ChainType::Testnet,
        ChainType::Testnet => ChainType::Regtest(ActivationHeights::default()),
        ChainType::Regtest(_) => ChainType::Mainnet,
    }
}

fn receiver_selection_strategy() -> impl Strategy<Value = ReceiverSelection> {
    (any::<bool>(), any::<bool>())
        .prop_map(|(orchard, sapling)| ReceiverSelection { orchard, sapling })
}

fn unified_addresses_strategy()
-> impl Strategy<Value = BTreeMap<UnifiedAddressId, ReceiverSelection>> {
    prop::collection::vec(
        (
            Just(AccountId::ZERO),
            any::<u32>(),
            receiver_selection_strategy(),
        ),
        0..8,
    )
    .prop_map(|entries| {
        entries
            .into_iter()
            .map(|(account_id, address_index, receivers)| {
                (
                    UnifiedAddressId {
                        account_id,
                        address_index,
                    },
                    receivers,
                )
            })
            .collect()
    })
}

fn transparent_scope_strategy() -> impl Strategy<Value = TransparentScope> {
    prop_oneof![
        Just(TransparentScope::External),
        Just(TransparentScope::Internal),
        Just(TransparentScope::Refund),
    ]
}

fn transparent_addresses_strategy() -> impl Strategy<Value = BTreeSet<TransparentAddressId>> {
    prop::collection::vec(
        (
            Just(AccountId::ZERO),
            transparent_scope_strategy(),
            0u32..=0x7FFF_FFFF,
        ),
        0..8,
    )
    .prop_map(|entries| {
        entries
            .into_iter()
            .map(|(account_id, scope, index)| {
                TransparentAddressId::new(
                    account_id,
                    scope,
                    NonHardenedChildIndex::from_index(index).unwrap(),
                )
            })
            .collect()
    })
}

fn performance_level_strategy() -> impl Strategy<Value = PerformanceLevel> {
    prop_oneof![
        Just(PerformanceLevel::Low),
        Just(PerformanceLevel::Medium),
        Just(PerformanceLevel::High),
        Just(PerformanceLevel::Maximum),
    ]
}

/// `SyncConfig::read` rejects a capacity outside this range (the channel allocates every slot
/// up front and panics on zero, so both ends are checked on read).
const EVENT_CHANNEL_CAPACITY_RANGE: std::ops::RangeInclusive<u64> = 1..=65_536;

fn wallet_settings_strategy() -> impl Strategy<Value = WalletSettings> {
    (
        any::<u8>(),
        any::<bool>(),
        any::<bool>(),
        any::<bool>(),
        performance_level_strategy(),
        EVENT_CHANNEL_CAPACITY_RANGE,
        1u32..=u32::MAX,
    )
        .prop_map(
            |(
                gap_limit,
                external,
                internal,
                refund,
                performance_level,
                event_channel_capacity,
                min_confirmations,
            )| {
                WalletSettings {
                    sync_config: SyncConfig {
                        transparent_address_discovery: TransparentAddressDiscovery {
                            gap_limit,
                            scopes: TransparentAddressDiscoveryScopes {
                                external,
                                internal,
                                refund,
                            },
                        },
                        performance_level,
                        event_channel_capacity: event_channel_capacity as usize,
                    },
                    min_confirmations: NonZeroU32::new(min_confirmations).unwrap(),
                }
            },
        )
}

fn price_strategy() -> impl Strategy<Value = Price> {
    (any::<u32>(), any::<f32>()).prop_map(|(time, price_usd)| Price { time, price_usd })
}

/// Builds a `PriceList` with the given fields via `PriceList`'s own wire format and public
/// `read`, since there is no public constructor for arbitrary field values. See the module doc.
fn build_price_list(
    time_last_updated: Option<u32>,
    current_price: Option<Price>,
    daily_prices: &[Price],
) -> PriceList {
    let mut encoded = Vec::new();
    encoded
        .write_u8(<PriceList as ReadableWriteable>::VERSION)
        .unwrap();
    Optional::write(&mut encoded, time_last_updated, |w: &mut Vec<u8>, time| {
        w.write_u32::<LittleEndian>(time)
    })
    .unwrap();
    Optional::write(
        &mut encoded,
        current_price,
        |w: &mut Vec<u8>, price: Price| {
            w.write_u32::<LittleEndian>(price.time)?;
            w.write_f32::<LittleEndian>(price.price_usd)
        },
    )
    .unwrap();
    Vector::write(&mut encoded, daily_prices, |w, price: &Price| {
        w.write_u32::<LittleEndian>(price.time)?;
        w.write_f32::<LittleEndian>(price.price_usd)
    })
    .unwrap();
    PriceList::read(encoded.as_slice(), ()).unwrap()
}

fn price_list_strategy() -> impl Strategy<Value = PriceList> {
    (
        prop::option::of(any::<u32>()),
        prop::option::of(price_strategy()),
        prop::collection::vec(price_strategy(), 0..8),
    )
        .prop_map(|(time_last_updated, current_price, daily_prices)| {
            build_price_list(time_last_updated, current_price, &daily_prices)
        })
}

/// Above every chain's Sapling activation height (mainnet's, the highest, is 419,200), since
/// `WalletFile::read` rejects a scan target below the chain's own activation height.
const MIN_SCAN_TARGET_HEIGHT: u32 = 500_000;

fn scan_target_strategy() -> impl Strategy<Value = ScanTarget> {
    (
        MIN_SCAN_TARGET_HEIGHT..1_500_000,
        any::<[u8; 32]>(),
        any::<bool>(),
    )
        .prop_map(|(height, txid_bytes, narrow_scan_area)| ScanTarget {
            block_height: BlockHeight::from_u32(height),
            txid: TxId::from_bytes(txid_bytes),
            narrow_scan_area,
        })
}

/// One scan range per [`ScanPriority`], laid end to end in that order starting from an arbitrary
/// height, so every case exercises the full set of priorities `SyncState::read` understands
/// while keeping the ranges contiguous, which `SyncState::read` also requires.
fn scan_ranges_strategy() -> impl Strategy<Value = Vec<(Range<BlockHeight>, ScanPriority)>> {
    (0u32..1_000_000, prop::array::uniform(0u32..1_000)).prop_map(
        |(start, lengths): (u32, [u32; SCAN_PRIORITIES.len()])| {
            let mut cursor = start;
            SCAN_PRIORITIES
                .iter()
                .zip(lengths)
                .map(|(&priority, len)| {
                    let range = BlockHeight::from_u32(cursor)..BlockHeight::from_u32(cursor + len);
                    cursor += len;
                    (range, priority)
                })
                .collect()
        },
    )
}

/// Builds a `SyncState` from the given scan ranges and scan targets via `SyncState`'s own wire
/// format and public `read`. See the module doc for why: `from_parts` is `pub(crate)`.
fn build_sync_state(
    scan_ranges: &[(Range<BlockHeight>, ScanPriority)],
    scan_targets: &BTreeSet<ScanTarget>,
) -> SyncState {
    let mut encoded = Vec::new();
    encoded
        .write_u8(<SyncState as ReadableWriteable>::VERSION)
        .unwrap();
    Vector::write(&mut encoded, scan_ranges, |w, (range, priority)| {
        w.write_u32::<LittleEndian>(range.start.into())?;
        w.write_u32::<LittleEndian>(range.end.into())?;
        w.write_u8(scan_priority_tag(*priority))
    })
    .unwrap();
    let no_shard_ranges: Vec<Range<BlockHeight>> = Vec::new();
    for _ in 0..3 {
        Vector::write(
            &mut encoded,
            &no_shard_ranges,
            |w, range: &Range<BlockHeight>| {
                w.write_u32::<LittleEndian>(range.start.into())?;
                w.write_u32::<LittleEndian>(range.end.into())
            },
        )
        .unwrap();
    }
    Vector::write(
        &mut encoded,
        &scan_targets.iter().collect::<Vec<_>>(),
        |w, &&target| target.write(w, ()),
    )
    .unwrap();
    SyncState::read(encoded.as_slice(), ()).unwrap()
}

fn sync_state_strategy() -> impl Strategy<Value = SyncState> {
    (
        scan_ranges_strategy(),
        prop::collection::btree_set(scan_target_strategy(), 0..5),
    )
        .prop_map(|(scan_ranges, scan_targets)| build_sync_state(&scan_ranges, &scan_targets))
}

fn output_id_strategy() -> impl Strategy<Value = OutputId> {
    (any::<[u8; 32]>(), any::<u32>()).prop_map(|(txid_bytes, output_index)| {
        OutputId::new(TxId::from_bytes(txid_bytes), output_index)
    })
}

fn outpoint_map_strategy() -> impl Strategy<Value = BTreeMap<OutputId, ScanTarget>> {
    prop::collection::btree_map(output_id_strategy(), scan_target_strategy(), 0..5)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(24))]

    /// Write, read, write again: the second write must reproduce the first byte for byte.
    /// `read` and `read_any` must land on the same bytes too, and the same bytes read against a
    /// different chain must fail.
    #[test]
    fn no_chain_dependent_fields_survive_a_round_trip(
        chain_type in chain_type_strategy(),
        unified_addresses in unified_addresses_strategy(),
        transparent_addresses in transparent_addresses_strategy(),
        wallet_settings in wallet_settings_strategy(),
        price_list in price_list_strategy(),
        sync_state in sync_state_strategy(),
        outpoint_map in outpoint_map_strategy(),
    ) {
        let mut file = fresh(chain_type, BASE_SEED);
        file.unified_addresses = unified_addresses;
        file.transparent_addresses = transparent_addresses;
        file.wallet_settings = wallet_settings;
        file.price_list = price_list;
        file.sync_state = sync_state;
        file.outpoint_map = outpoint_map;

        let written = bytes(&file);

        let read_back = WalletFile::read(written.as_slice(), chain_type).unwrap();
        prop_assert_eq!(bytes(&read_back), written.clone(), "write -> read -> write is not a fixed point");

        let read_any_back = WalletFile::read_any(written.as_slice()).unwrap();
        prop_assert_eq!(bytes(&read_any_back), written.clone(), "read and read_any disagree after rewrite");

        let wrong_chain = a_different_chain(chain_type);
        prop_assert!(
            WalletFile::read(written.as_slice(), wrong_chain).is_err(),
            "wallet written for {chain_type} was accepted when read as {wrong_chain}"
        );
    }
}

fn mismatch(a: &[u8], b: &[u8]) -> usize {
    a.iter()
        .zip(b)
        .position(|(x, y)| x != y)
        .unwrap_or(a.len().min(b.len()))
}

/// A small integer as 32 little-endian bytes: below every field modulus in play, so it decodes
/// as a Pallas base element, a Jubjub base element or a nullifier alike.
fn field_bytes(value: u64) -> [u8; 32] {
    let mut bytes = [0u8; 32];
    bytes[..8].copy_from_slice(&value.to_le_bytes());
    bytes
}

fn txid_strategy() -> impl Strategy<Value = TxId> {
    any::<[u8; 32]>().prop_map(TxId::from_bytes)
}

fn status_strategy() -> impl Strategy<Value = ConfirmationStatus> {
    (0u8..5, any::<u32>().prop_map(BlockHeight::from_u32)).prop_map(|(kind, height)| match kind {
        0 => ConfirmationStatus::Confirmed(height),
        1 => ConfirmationStatus::Mempool(height),
        2 => ConfirmationStatus::Transmitted(height),
        3 => ConfirmationStatus::Calculated(height),
        _ => ConfirmationStatus::Failed(height),
    })
}

fn scope_strategy() -> impl Strategy<Value = Scope> {
    prop_oneof![Just(Scope::External), Just(Scope::Internal)]
}

fn memo_strategy() -> impl Strategy<Value = Memo> {
    prop_oneof![
        Just(Memo::Empty),
        "[ -~]{1,120}".prop_map(|text| text.parse().unwrap()),
        any::<[u8; 32]>().prop_map(|head| {
            let mut bytes = [0u8; 511];
            bytes[..32].copy_from_slice(&head);
            Memo::Arbitrary(Box::new(bytes))
        }),
    ]
}

/// A received note, before the recipient is known: everything the wire format carries except
/// the address, which comes from the wallet's own keys when the note is built.
#[derive(Debug, Clone)]
struct NoteSpec {
    output_index: u32,
    scope: Scope,
    value: u64,
    seed: u64,
    rseed: [u8; 32],
    memo: Memo,
    nullifier: Option<u64>,
    position: Option<u64>,
    spending_transaction: Option<TxId>,
}

fn note_spec_strategy() -> impl Strategy<Value = NoteSpec> {
    (
        any::<u32>(),
        scope_strategy(),
        0..=MAX_MONEY,
        any::<u64>(),
        any::<[u8; 32]>(),
        memo_strategy(),
        prop::option::of(any::<u64>()),
        prop::option::of(any::<u64>()),
        prop::option::of(txid_strategy()),
    )
        .prop_map(
            |(
                output_index,
                scope,
                value,
                seed,
                rseed,
                memo,
                nullifier,
                position,
                spending_transaction,
            )| NoteSpec {
                output_index,
                scope,
                value,
                seed,
                rseed,
                memo,
                nullifier,
                position,
                spending_transaction,
            },
        )
}

#[derive(Debug, Clone)]
struct OutgoingSpec {
    output_index: u32,
    scope: Scope,
    value: u64,
    seed: u64,
    rseed: [u8; 32],
    memo: Memo,
    to_self: bool,
}

fn outgoing_spec_strategy() -> impl Strategy<Value = OutgoingSpec> {
    (
        any::<u32>(),
        scope_strategy(),
        0..=MAX_MONEY,
        any::<u64>(),
        any::<[u8; 32]>(),
        memo_strategy(),
        any::<bool>(),
    )
        .prop_map(
            |(output_index, scope, value, seed, rseed, memo, to_self)| OutgoingSpec {
                output_index,
                scope,
                value,
                seed,
                rseed,
                memo,
                to_self,
            },
        )
}

#[derive(Debug, Clone)]
struct CoinSpec {
    output_index: u32,
    address_index: u32,
    scope: TransparentScope,
    value: u64,
    spending_transaction: Option<TxId>,
}

fn coin_spec_strategy() -> impl Strategy<Value = CoinSpec> {
    (
        any::<u32>(),
        0u32..=0x7FFF_FFFF,
        transparent_scope_strategy(),
        0..=MAX_MONEY,
        prop::option::of(txid_strategy()),
    )
        .prop_map(
            |(output_index, address_index, scope, value, spending_transaction)| CoinSpec {
                output_index,
                address_index,
                scope,
                value,
                spending_transaction,
            },
        )
}

#[derive(Debug, Clone)]
struct TransactionSpec {
    txid: TxId,
    status: ConfirmationStatus,
    coins: Vec<CoinSpec>,
    sapling: Vec<NoteSpec>,
    orchard: Vec<NoteSpec>,
    ironwood: Vec<NoteSpec>,
    outgoing_sapling: Vec<OutgoingSpec>,
    outgoing_orchard: Vec<OutgoingSpec>,
    outgoing_ironwood: Vec<OutgoingSpec>,
}

fn transaction_spec_strategy() -> impl Strategy<Value = TransactionSpec> {
    (
        txid_strategy(),
        status_strategy(),
        prop::collection::vec(coin_spec_strategy(), 0..3),
        prop::collection::vec(note_spec_strategy(), 0..3),
        prop::collection::vec(note_spec_strategy(), 0..3),
        prop::collection::vec(note_spec_strategy(), 0..3),
        prop::collection::vec(outgoing_spec_strategy(), 0..3),
        prop::collection::vec(outgoing_spec_strategy(), 0..3),
        prop::collection::vec(outgoing_spec_strategy(), 0..3),
    )
        .prop_map(
            |(
                txid,
                status,
                coins,
                sapling,
                orchard,
                ironwood,
                outgoing_sapling,
                outgoing_orchard,
                outgoing_ironwood,
            )| TransactionSpec {
                txid,
                status,
                coins,
                sapling,
                orchard,
                ironwood,
                outgoing_sapling,
                outgoing_orchard,
                outgoing_ironwood,
            },
        )
}

#[derive(Debug, Clone)]
struct BlockSpec {
    height: u32,
    hash: [u8; 32],
    prev_hash: [u8; 32],
    time: u32,
    txids: Vec<TxId>,
    tree_bounds: [(u32, u32); 3],
}

/// Tree bounds are an initial size and a growth per pool, since the reader rejects a final
/// size below the initial one.
fn block_spec_strategy() -> impl Strategy<Value = BlockSpec> {
    (
        any::<u32>(),
        any::<[u8; 32]>(),
        any::<[u8; 32]>(),
        any::<u32>(),
        prop::collection::vec(txid_strategy(), 0..4),
        prop::array::uniform3((0..u32::MAX / 2, 0..u32::MAX / 2)),
    )
        .prop_map(
            |(height, hash, prev_hash, time, txids, tree_bounds)| BlockSpec {
                height,
                hash,
                prev_hash,
                time,
                txids,
                tree_bounds,
            },
        )
}

/// What a leaf keeps when the tree prunes. Checkpoint ids are the leaf's index plus one, so
/// they increase as the leaves are appended and stay above the height-zero checkpoint a new
/// tree starts with, both of which the shard tree requires.
#[derive(Debug, Clone, Copy)]
enum Leaf {
    Ephemeral,
    Marked,
    Checkpoint,
    MarkedCheckpoint,
}

fn leaves_strategy() -> impl Strategy<Value = Vec<(u64, Leaf)>> {
    prop::collection::vec(
        (
            1u64..,
            prop_oneof![
                Just(Leaf::Ephemeral),
                Just(Leaf::Marked),
                Just(Leaf::Checkpoint),
                Just(Leaf::MarkedCheckpoint),
            ],
        ),
        0..MAX_TREE_LEAVES,
    )
}

fn retention(index: usize, leaf: Leaf) -> Retention<BlockHeight> {
    let id = BlockHeight::from_u32(index as u32 + 1);
    match leaf {
        Leaf::Ephemeral => Retention::Ephemeral,
        Leaf::Marked => Retention::Marked,
        Leaf::Checkpoint => Retention::Checkpoint {
            id,
            marking: Marking::None,
        },
        Leaf::MarkedCheckpoint => Retention::Checkpoint {
            id,
            marking: Marking::Marked,
        },
    }
}

fn build_trees(
    sapling: &[(u64, Leaf)],
    orchard: &[(u64, Leaf)],
    ironwood: &[(u64, Leaf)],
) -> ShardTrees {
    let mut trees = ShardTrees::new();
    for (index, &(value, leaf)) in sapling.iter().enumerate() {
        let node = sapling_crypto::Node::from_bytes(field_bytes(value)).unwrap();
        trees.sapling.append(node, retention(index, leaf)).unwrap();
    }
    for (index, &(value, leaf)) in orchard.iter().enumerate() {
        let node = MerkleHashOrchard::from_bytes(&field_bytes(value)).unwrap();
        trees.orchard.append(node, retention(index, leaf)).unwrap();
    }
    for (index, &(value, leaf)) in ironwood.iter().enumerate() {
        let node = MerkleHashOrchard::from_bytes(&field_bytes(value)).unwrap();
        trees.ironwood.append(node, retention(index, leaf)).unwrap();
    }
    trees
}

fn build_block(spec: &BlockSpec) -> WalletBlock {
    let mut encoded = vec![<WalletBlock as ReadableWriteable>::VERSION];
    encoded.write_u32::<LittleEndian>(spec.height).unwrap();
    encoded.extend_from_slice(&spec.hash);
    encoded.extend_from_slice(&spec.prev_hash);
    encoded.write_u32::<LittleEndian>(spec.time).unwrap();
    Vector::write(&mut encoded, &spec.txids, |w, txid| txid.write(w)).unwrap();
    encoded.push(1);
    for (initial, growth) in spec.tree_bounds {
        encoded.write_u32::<LittleEndian>(initial).unwrap();
        encoded.write_u32::<LittleEndian>(initial + growth).unwrap();
    }
    WalletBlock::read(encoded.as_slice(), ()).unwrap()
}

/// The addresses the wallet's own keys derive, which every received and self-sent note points
/// at.
struct Receivers {
    sapling: sapling_crypto::PaymentAddress,
    orchard: orchard::Address,
    unified: String,
}

fn receivers(keys: &UnifiedKeyStore, chain_type: &ChainType) -> Receivers {
    let own = keys
        .generate_unified_address(0, ReceiverSelection::all_shielded())
        .unwrap();
    Receivers {
        sapling: *own.sapling().unwrap(),
        orchard: *own.orchard().unwrap(),
        unified: own.encode(chain_type),
    }
}

fn sapling_rseed(seed: u64, rseed: [u8; 32]) -> sapling_crypto::Rseed {
    if seed.is_multiple_of(2) {
        sapling_crypto::Rseed::AfterZip212(rseed)
    } else {
        sapling_crypto::Rseed::BeforeZip212(jubjub::Fr::from(seed))
    }
}

fn sapling_note(
    recipient: sapling_crypto::PaymentAddress,
    value: u64,
    seed: u64,
    rseed: [u8; 32],
) -> sapling_crypto::Note {
    sapling_crypto::Note::from_parts(
        recipient,
        sapling_crypto::value::NoteValue::from_raw(value),
        sapling_rseed(seed, rseed),
    )
}

/// A random seed is rejected for a vanishing fraction of (rho, rseed) pairs; the next seed is
/// tried so the strategy never has to know.
fn orchard_note(
    recipient: orchard::Address,
    value: u64,
    seed: u64,
    mut rseed: [u8; 32],
    version: NoteVersion,
) -> orchard::Note {
    let rho = Rho::from_bytes(&field_bytes(seed)).unwrap();
    loop {
        let note = Option::from(RandomSeed::from_bytes(rseed, &rho)).and_then(|rseed| {
            Option::from(orchard::Note::from_parts(
                recipient,
                NoteValue::from_raw(value),
                rho,
                rseed,
                version,
            ))
        });
        if let Some(note) = note {
            return note;
        }
        rseed[0] = rseed[0].wrapping_add(1);
    }
}

fn orchard_nullifier(seed: u64) -> orchard::note::Nullifier {
    orchard::note::Nullifier::from_bytes(&field_bytes(seed)).unwrap()
}

fn received<N, Nf: Copy, P>(
    spec: &NoteSpec,
    txid: TxId,
    note: N,
    nullifier: Option<Nf>,
) -> WalletNote<N, Nf, P>
where
    WalletNote<N, Nf, P>: OutputInterface,
{
    let mut wallet_note = WalletNote::new_for_test(
        OutputId::new(txid, spec.output_index),
        AccountId::ZERO,
        spec.scope,
        note,
        spec.memo.clone(),
        spec.position.map(Position::from),
    );
    if let Some(nullifier) = nullifier {
        wallet_note = wallet_note.with_nullifier_for_test(nullifier);
    }
    wallet_note.set_spending_transaction(spec.spending_transaction);
    wallet_note
}

/// The outgoing note record: version, output id, key id, then the pool's note fields, the memo
/// and the optional recipient unified address.
fn outgoing_record(spec: &OutgoingSpec, txid: TxId, note: &[u8], to_self: &str) -> Vec<u8> {
    let mut out = vec![1];
    OutputId::new(txid, spec.output_index)
        .write(&mut out)
        .unwrap();
    KeyId {
        account_id: AccountId::ZERO,
        scope: spec.scope,
    }
    .write(&mut out)
    .unwrap();
    out.extend_from_slice(note);
    out.extend_from_slice(spec.memo.encode().as_array());
    Optional::write(&mut out, spec.to_self.then_some(to_self), |w, address| {
        w.write_u64::<LittleEndian>(address.len() as u64)?;
        w.write_all(address.as_bytes())
    })
    .unwrap();
    out
}

fn outgoing_sapling_record(spec: &OutgoingSpec, txid: TxId, receivers: &Receivers) -> Vec<u8> {
    let note = sapling_note(receivers.sapling, spec.value, spec.seed, spec.rseed);
    let mut fields = note.recipient().to_bytes().to_vec();
    fields.write_u64::<LittleEndian>(spec.value).unwrap();
    match note.rseed() {
        sapling_crypto::Rseed::BeforeZip212(fr) => {
            fields.push(0);
            fields.extend_from_slice(&fr.to_bytes());
        }
        sapling_crypto::Rseed::AfterZip212(bytes) => {
            fields.push(1);
            fields.extend_from_slice(bytes);
        }
    }
    outgoing_record(spec, txid, &fields, &receivers.unified)
}

fn outgoing_orchard_record(
    spec: &OutgoingSpec,
    txid: TxId,
    receivers: &Receivers,
    version: NoteVersion,
) -> Vec<u8> {
    let note = orchard_note(
        receivers.orchard,
        spec.value,
        spec.seed,
        spec.rseed,
        version,
    );
    let mut fields = note.recipient().to_raw_address_bytes().to_vec();
    fields.write_u64::<LittleEndian>(spec.value).unwrap();
    fields.extend_from_slice(&note.rho().to_bytes());
    fields.extend_from_slice(note.rseed().as_bytes());
    outgoing_record(spec, txid, &fields, &receivers.unified)
}

fn coin(
    spec: &CoinSpec,
    txid: TxId,
    keys: &UnifiedKeyStore,
    chain_type: &ChainType,
) -> TransparentCoin {
    let index = NonHardenedChildIndex::from_index(spec.address_index).unwrap();
    let address = keys
        .generate_transparent_address(index, spec.scope)
        .unwrap();
    let mut coin = TransparentCoin::new_for_test(
        OutputId::new(txid, spec.output_index),
        TransparentAddressId::new(AccountId::ZERO, spec.scope, index),
        encode_address(chain_type, address),
        TransparentAddress::script(&address).into(),
        Zatoshis::from_u64(spec.value).unwrap(),
    );
    coin.set_spending_transaction(spec.spending_transaction);
    coin
}

fn transaction(
    spec: &TransactionSpec,
    keys: &UnifiedKeyStore,
    receivers: &Receivers,
    chain_type: &ChainType,
) -> WalletTransaction {
    let txid = spec.txid;
    let sapling_notes = spec
        .sapling
        .iter()
        .map(|note| {
            received::<_, _, _>(
                note,
                txid,
                sapling_note(receivers.sapling, note.value, note.seed, note.rseed),
                note.nullifier
                    .map(|seed| sapling_crypto::Nullifier(field_bytes(seed))),
            )
        })
        .collect::<Vec<SaplingNote>>();
    let orchard_notes = spec
        .orchard
        .iter()
        .map(|note| {
            received(
                note,
                txid,
                orchard_note(
                    receivers.orchard,
                    note.value,
                    note.seed,
                    note.rseed,
                    NoteVersion::V2,
                ),
                note.nullifier.map(orchard_nullifier),
            )
        })
        .collect::<Vec<OrchardNote>>();
    let ironwood_notes = spec
        .ironwood
        .iter()
        .map(|note| {
            received(
                note,
                txid,
                orchard_note(
                    receivers.orchard,
                    note.value,
                    note.seed,
                    note.rseed,
                    NoteVersion::V3,
                ),
                note.nullifier.map(orchard_nullifier),
            )
        })
        .collect::<Vec<IronwoodNote>>();
    let outgoing_sapling = spec
        .outgoing_sapling
        .iter()
        .map(|note| {
            OutgoingSaplingNote::read(
                outgoing_sapling_record(note, txid, receivers).as_slice(),
                chain_type,
            )
            .unwrap()
        })
        .collect();
    let outgoing_orchard = spec
        .outgoing_orchard
        .iter()
        .map(|note| {
            OutgoingOrchardNote::read(
                outgoing_orchard_record(note, txid, receivers, NoteVersion::V2).as_slice(),
                chain_type,
            )
            .unwrap()
        })
        .collect();
    let outgoing_ironwood = spec
        .outgoing_ironwood
        .iter()
        .map(|note| {
            OutgoingIronwoodNote::read(
                outgoing_orchard_record(note, txid, receivers, NoteVersion::V3).as_slice(),
                chain_type,
            )
            .unwrap()
        })
        .collect();
    let coins = spec
        .coins
        .iter()
        .map(|spec| coin(spec, txid, keys, chain_type))
        .collect();
    WalletTransaction::new_for_test_with_orchard_notes(
        txid,
        spec.status,
        orchard_notes,
        outgoing_orchard,
    )
    .with_sapling_notes_for_test(sapling_notes)
    .with_outgoing_sapling_notes_for_test(outgoing_sapling)
    .with_transparent_coins_for_test(coins)
    .with_ironwood_notes_for_test(ironwood_notes, outgoing_ironwood)
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// The scanned data of a wallet, written, read and written again, plain and through an
    /// encrypted envelope.
    #[test]
    fn scanned_data_survives_a_round_trip(
        chain_type in chain_type_strategy(),
        blocks in prop::collection::vec(block_spec_strategy(), 0..4),
        transactions in prop::collection::vec(transaction_spec_strategy(), 0..4),
        sapling_nullifiers in prop::collection::btree_map(any::<[u8; 32]>(), scan_target_strategy(), 0..4),
        orchard_nullifiers in prop::collection::btree_map(any::<u64>(), scan_target_strategy(), 0..4),
        ironwood_nullifiers in prop::collection::btree_map(any::<u64>(), scan_target_strategy(), 0..4),
        sapling_leaves in leaves_strategy(),
        orchard_leaves in leaves_strategy(),
        ironwood_leaves in leaves_strategy(),
    ) {
        let mut file = fresh(chain_type, BASE_SEED);
        let keys = &file.unified_key_store[&AccountId::ZERO];
        let receivers = receivers(keys, &chain_type);
        let transactions = transactions
            .iter()
            .map(|spec| transaction(spec, keys, &receivers, &chain_type))
            .map(|transaction| (transaction.txid(), transaction))
            .collect();
        file.wallet_transactions = transactions;
        file.wallet_blocks = blocks
            .iter()
            .map(build_block)
            .map(|block| (block.block_height(), block))
            .collect();
        file.nullifier_map.sapling = sapling_nullifiers
            .into_iter()
            .map(|(nullifier, target)| (sapling_crypto::Nullifier(nullifier), target))
            .collect();
        file.nullifier_map.orchard = orchard_nullifiers
            .into_iter()
            .map(|(seed, target)| (orchard_nullifier(seed), target))
            .collect();
        file.nullifier_map.ironwood = ironwood_nullifiers
            .into_iter()
            .map(|(seed, target)| (orchard_nullifier(seed), target))
            .collect();
        file.shard_trees = build_trees(&sapling_leaves, &orchard_leaves, &ironwood_leaves);

        let written = bytes(&file);

        let read_back = WalletFile::read(written.as_slice(), chain_type).unwrap();
        let rewritten = bytes(&read_back);
        prop_assert!(
            rewritten == written,
            "write -> read -> write differs at byte {}",
            mismatch(&rewritten, &written)
        );

        let envelope = fast_session(PASSPHRASE).encrypt(&written).unwrap();
        let (decrypted, _) =
            WalletFile::read_encrypted_any(envelope.as_slice(), Some(PASSPHRASE.to_string())).unwrap();
        let rewritten = bytes(&decrypted);
        prop_assert!(
            rewritten == written,
            "write -> encrypt -> read -> write differs at byte {}",
            mismatch(&rewritten, &written)
        );
    }
}
