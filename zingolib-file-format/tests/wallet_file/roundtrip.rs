//! Property tests over every part of the wallet file that has no chain dependency: unified and
//! transparent address ids, wallet settings, the price list, sync state and the outpoint map.
//!
//! `SyncState` and `PriceList` have no public constructor that takes arbitrary field values
//! (`SyncState::from_parts` is `pub(crate)` to pepper-sync, and `PriceList`'s fields are
//! private), so both are built the same way a real reader builds them: encode the fields as the
//! documented wire format and decode with the type's own public `read`.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;
use std::ops::Range;

use byteorder::{LittleEndian, WriteBytesExt};
use proptest::prelude::*;

use zcash_encoding::{Optional, Vector};
use zcash_primitives::transaction::TxId;
use zcash_protocol::consensus::BlockHeight;
use zcash_transparent::keys::NonHardenedChildIndex;
use zip32::AccountId;

use pepper_sync::config::{
    PerformanceLevel, SyncConfig, TransparentAddressDiscovery, TransparentAddressDiscoveryScopes,
};
use pepper_sync::keys::transparent::TransparentAddressId;
use pepper_sync::sync::ScanPriority;
use pepper_sync::wallet::{OutputId, ScanTarget, SyncState};
use zingo_common_components::protocol::ActivationHeights;
use zingolib_common::chain::ChainType;
use zingolib_common::keys::{ReceiverSelection, TransparentScope, UnifiedAddressId};
use zingolib_common::serialization::ReadableWriteable;
use zingolib_file_format::{WalletFile, WalletSettings};
use zingolib_price::{Price, PriceList};

use super::support::{bytes, fresh};

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
