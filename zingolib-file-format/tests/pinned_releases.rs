//! Wallet files written by the two pendrake-watch releases, read back by this build.
//!
//! v0.0.1 was built from zingolib `ad66f31e821357ba231608f0c487d92dfc6884e9` and v0.0.2 from
//! `e645fd031c6f16dc8eb9b8c39287a27f7731ba8b`. Both write layout 41. The files under
//! `tests/vectors` were written once by a throwaway integration test,
//! `zingolib/tests/pinned_vectors.rs`, run in a scratch worktree of each revision. It built the
//! scenarios below with that revision's `LightWallet` and pepper-sync test constructors, wrote
//! the plain copies with `LightWallet::write` and the encrypted copies with `LightWallet::save`
//! after `set_passphrase_with_params` with the passphrase `conformance` and Argon2 m_cost 8,
//! t_cost 1, p_cost 1. v0.0.1 ships only `WalletTransaction::new_for_test`, so its worktree
//! carried v0.0.2's note, coin, transaction and sync state test constructors, copied in without
//! touching any serialization code.
//!
//! Each test builds the same wallets here and asserts that reading a vector and writing it back
//! gives exactly the bytes this build writes for the fresh wallet. v0.0.2 already writes the
//! current struct versions, so its raw bytes must equal those bytes as well. v0.0.1 writes older
//! struct versions, so its raw bytes must not.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    io::Write,
    num::NonZeroU32,
};

use bip0039::Mnemonic;
use byteorder::{LittleEndian, WriteBytesExt};
use incrementalmerkletree::{Marking, Position, Retention};
use orchard::{
    note::{NoteVersion, RandomSeed, Rho},
    tree::MerkleHashOrchard,
    value::NoteValue,
};
use secrecy::SecretString;
use shardtree::store::ShardStore;
use zcash_encoding::{CompactSize, Optional, Vector};
use zcash_keys::keys::UnifiedSpendingKey;
use zcash_primitives::transaction::TxId;
use zcash_protocol::{consensus::BlockHeight, memo::Memo, value::Zatoshis};
use zcash_transparent::{address::TransparentAddress, keys::NonHardenedChildIndex};
use zingo_common_components::protocol::ActivationHeights;
use zip32::{AccountId, Scope};

use pepper_sync::{
    add_scan_targets,
    config::{
        PerformanceLevel, SyncConfig, TransparentAddressDiscovery,
        TransparentAddressDiscoveryScopes,
    },
    keys::{
        KeyId,
        transparent::{TransparentAddressId, encode_address},
    },
    sync::{ScanPriority, ScanRange},
    wallet::{
        IronwoodNote, NullifierMap, OrchardNote, OutgoingIronwoodNote, OutgoingOrchardNote,
        OutputId, OutputInterface, SaplingNote, ScanTarget, ShardTrees, SyncState, TransparentCoin,
        WalletTransaction,
    },
};
use zingolib_common::{
    chain::ChainType,
    keys::{ReceiverSelection, TransparentScope, UnifiedAddressId, UnifiedKeyStore},
    serialization::ReadableWriteable,
    status::ConfirmationStatus,
};
use zingolib_file_format::{WalletFile, WalletFileRef, WalletSettings, encryption};
use zingolib_price::PriceList;

const PASSPHRASE: &str = "conformance";
const BIRTHDAY: u32 = 2_000_000;
/// Leaves appended to each note commitment tree, each with its own checkpoint. Above
/// pepper-sync's reorg allowance of 100, so the trees prune their oldest checkpoints.
const TREE_LEAVES: u32 = 110;

#[derive(Clone, Copy)]
enum Release {
    V001,
    V002,
}

struct TestVector {
    name: &'static str,
    chain: &'static str,
    bytes: &'static [u8],
}

/// The six files of one scenario: each chain, plain and encrypted.
macro_rules! vectors {
    ($release:literal, $scenario:literal) => {
        [
            vectors!(@one $release, $scenario, "mainnet", ""),
            vectors!(@one $release, $scenario, "mainnet", "-encrypted"),
            vectors!(@one $release, $scenario, "testnet", ""),
            vectors!(@one $release, $scenario, "testnet", "-encrypted"),
            vectors!(@one $release, $scenario, "regtest", ""),
            vectors!(@one $release, $scenario, "regtest", "-encrypted"),
        ]
    };
    (@one $release:literal, $scenario:literal, $chain:literal, $suffix:literal) => {
        TestVector {
            name: concat!($release, "/", $scenario, "-", $chain, $suffix, ".dat"),
            chain: $chain,
            bytes: include_bytes!(concat!(
                "vectors/", $release, "/", $scenario, "-", $chain, $suffix, ".dat"
            )),
        }
    };
}

fn chain(name: &str) -> ChainType {
    match name {
        "mainnet" => ChainType::Mainnet,
        "testnet" => ChainType::Testnet,
        "regtest" => ChainType::Regtest(ActivationHeights::default()),
        _ => unreachable!("vectors only exist for the three chains"),
    }
}

fn height(offset: u32) -> BlockHeight {
    BlockHeight::from_u32(BIRTHDAY + offset)
}

fn txid(byte: u8) -> TxId {
    TxId::from_bytes([byte; 32])
}

fn mnemonic(byte: u8) -> Mnemonic {
    Mnemonic::from_entropy([byte; 32]).unwrap()
}

fn account(index: u32) -> AccountId {
    AccountId::try_from(index).unwrap()
}

fn unified_id(account_index: u32, address_index: u32) -> UnifiedAddressId {
    UnifiedAddressId {
        account_id: account(account_index),
        address_index,
    }
}

fn transparent_id(account_index: u32, scope: TransparentScope, index: u32) -> TransparentAddressId {
    TransparentAddressId::new(
        account(account_index),
        scope,
        NonHardenedChildIndex::from_index(index).unwrap(),
    )
}

/// A small field element, valid as an Orchard rho, nullifier or tree leaf and as a Sapling leaf.
fn field_bytes(value: u32) -> [u8; 32] {
    let mut bytes = [0u8; 32];
    bytes[..4].copy_from_slice(&value.to_le_bytes());
    bytes
}

fn write(file: &WalletFile) -> Vec<u8> {
    write_ref(WalletFileRef::from(file), &file.chain_type)
}

fn write_ref(file: WalletFileRef, chain_type: &ChainType) -> Vec<u8> {
    let mut out = Vec::new();
    file.write(&mut out, chain_type).unwrap();
    out
}

fn record(transaction: &WalletTransaction, chain_type: &ChainType) -> Vec<u8> {
    let mut out = Vec::new();
    transaction.write(&mut out, chain_type).unwrap();
    out
}

fn mismatch(a: &[u8], b: &[u8]) -> usize {
    a.iter()
        .zip(b)
        .position(|(x, y)| x != y)
        .unwrap_or(a.len().min(b.len()))
}

/// Offset of the transaction count in `wallet`'s file: writing it with no transactions and
/// with one gives a count byte of 0 and 1 there, after an identical prefix.
fn transactions_offset(wallet: &WalletFile) -> usize {
    let (txid, transaction) = wallet.wallet_transactions.iter().next().unwrap();
    let copy = WalletTransaction::read(
        record(transaction, &wallet.chain_type).as_slice(),
        &wallet.chain_type,
    )
    .unwrap();
    let one = HashMap::from([(*txid, copy)]);
    let none = HashMap::new();
    let with = |transactions| {
        let mut file = WalletFileRef::from(wallet);
        file.wallet_transactions = transactions;
        write_ref(file, &wallet.chain_type)
    };
    mismatch(&with(&none), &with(&one))
}

/// The released writers walked transactions out of a `HashMap`, so a vector lists them in
/// whatever order that run produced. HEAD writes them in txid order. This splits `written`
/// into `wallet`'s transaction records and puts them in txid order so the raw bytes of a
/// vector compare against HEAD's. Bytes that do not split that way come back unchanged, so
/// they still fail the comparison.
fn canonical(written: &[u8], wallet: &WalletFile) -> Vec<u8> {
    let records = wallet
        .wallet_transactions
        .iter()
        .collect::<BTreeMap<_, _>>()
        .into_values()
        .map(|transaction| record(transaction, &wallet.chain_type))
        .collect::<Vec<_>>();
    if records.len() < 2 {
        return written.to_vec();
    }
    let mut count = Vec::new();
    CompactSize::write(&mut count, records.len()).unwrap();
    let start = transactions_offset(wallet);
    let body = start + count.len();
    if written.get(start..body) != Some(count.as_slice()) {
        return written.to_vec();
    }
    let mut pending = records.iter().map(Vec::as_slice).collect::<Vec<_>>();
    let mut end = body;
    while !pending.is_empty() {
        let Some(found) = pending
            .iter()
            .position(|record| written[end..].starts_with(record))
        else {
            return written.to_vec();
        };
        end += pending.swap_remove(found).len();
    }
    [&written[..body], &records.concat(), &written[end..]].concat()
}

/// Checks every vector of one scenario against the wallet `build` makes for its chain and
/// returns each vector's decoded wallet.
fn check(
    release: Release,
    vectors: [TestVector; 6],
    build: impl Fn(ChainType) -> WalletFile,
) -> Vec<(&'static str, WalletFile)> {
    vectors
        .into_iter()
        .map(|vector| {
            let name = vector.name;
            let chain_type = chain(vector.chain);
            let encrypted = name.ends_with("-encrypted.dat");
            assert_eq!(
                encryption::is_encrypted(vector.bytes),
                encrypted,
                "{name}: envelope magic does not match the file name"
            );
            assert!(
                vector.bytes.len() < 200 * 1024,
                "{name}: vectors stay under 200 KB"
            );

            let fresh = build(chain_type);
            let expected = write(&fresh);

            let plaintext = if encrypted {
                let passphrase = SecretString::new(PASSPHRASE.to_string());
                let (plaintext, _) = encryption::decrypt(&passphrase, vector.bytes)
                    .unwrap_or_else(|e| panic!("{name}: decrypt failed: {e}"));
                for (reader, (file, session)) in [
                    (
                        "read_encrypted",
                        WalletFile::read_encrypted(
                            vector.bytes,
                            chain_type,
                            Some(PASSPHRASE.to_string()),
                        ),
                    ),
                    (
                        "read_encrypted_any",
                        WalletFile::read_encrypted_any(vector.bytes, Some(PASSPHRASE.to_string())),
                    ),
                ]
                .map(|(reader, outcome)| {
                    (
                        reader,
                        outcome.unwrap_or_else(|e| panic!("{name}: {reader} failed: {e}")),
                    )
                }) {
                    assert!(session.is_some(), "{name}: {reader} returned no session");
                    let rewrite = write(&file);
                    assert!(
                        rewrite == expected,
                        "{name}: {reader} rewrite differs from a fresh build at byte {}",
                        mismatch(&rewrite, &expected)
                    );
                }
                plaintext.to_vec()
            } else {
                vector.bytes.to_vec()
            };

            let file = WalletFile::read(plaintext.as_slice(), chain_type)
                .unwrap_or_else(|e| panic!("{name}: read failed: {e}"));
            let any = WalletFile::read_any(plaintext.as_slice())
                .unwrap_or_else(|e| panic!("{name}: read_any failed: {e}"));
            assert_eq!(file.read_version, WalletFile::VERSION, "{name}");
            assert_eq!(any.read_version, WalletFile::VERSION, "{name}");
            assert_eq!(
                any.chain_type.to_string(),
                vector.chain,
                "{name}: read_any recovered the wrong chain"
            );

            for (reader, decoded) in [("read", &file), ("read_any", &any)] {
                let rewrite = write(decoded);
                assert!(
                    rewrite == expected,
                    "{name}: {reader} rewrite differs from a fresh build at byte {}",
                    mismatch(&rewrite, &expected)
                );
            }

            let raw = canonical(&plaintext, &fresh);
            match release {
                Release::V001 => assert!(
                    raw != expected,
                    "{name}: v0.0.1 writes older struct versions, its raw bytes cannot match"
                ),
                Release::V002 => assert!(
                    raw == expected,
                    "{name}: raw bytes differ from a fresh build at byte {}",
                    mismatch(&raw, &expected)
                ),
            }
            (name, file)
        })
        .collect()
}

/// A wallet as `LightWallet::new` leaves it: account zero's first unified address with its
/// default receivers and its first external transparent address, defaults everywhere else.
fn wallet(
    chain_type: ChainType,
    mnemonic: Option<Mnemonic>,
    unified_key_store: BTreeMap<AccountId, UnifiedKeyStore>,
) -> WalletFile {
    let receivers = unified_key_store[&AccountId::ZERO]
        .default_receivers()
        .unwrap();
    WalletFile {
        read_version: WalletFile::VERSION,
        chain_type,
        mnemonic,
        birthday: height(0),
        unified_key_store,
        unified_addresses: BTreeMap::from([(unified_id(0, 0), receivers)]),
        transparent_addresses: BTreeSet::from([transparent_id(0, TransparentScope::External, 0)]),
        wallet_blocks: BTreeMap::new(),
        wallet_transactions: HashMap::new(),
        nullifier_map: NullifierMap::new(),
        outpoint_map: BTreeMap::new(),
        shard_trees: ShardTrees::new(),
        sync_state: SyncState::new(),
        wallet_settings: WalletSettings::default(),
        price_list: PriceList::new(),
    }
}

fn spend_keys(chain_type: ChainType, mnemonic: &Mnemonic, index: u32) -> UnifiedKeyStore {
    UnifiedKeyStore::new_from_mnemonic(chain_type, mnemonic, account(index)).unwrap()
}

fn empty(chain_type: ChainType) -> WalletFile {
    let keys = spend_keys(chain_type, &mnemonic(0x11), 0);
    wallet(
        chain_type,
        Some(mnemonic(0x11)),
        BTreeMap::from([(AccountId::ZERO, keys)]),
    )
}

fn view(chain_type: ChainType) -> WalletFile {
    let spend = spend_keys(chain_type, &mnemonic(0x22), 0);
    let ufvk = UnifiedSpendingKey::try_from(&spend)
        .unwrap()
        .to_unified_full_viewing_key()
        .encode(&chain_type);
    let keys = UnifiedKeyStore::new_from_ufvk(chain_type, ufvk).unwrap();
    wallet(chain_type, None, BTreeMap::from([(AccountId::ZERO, keys)]))
}

fn settings() -> WalletSettings {
    WalletSettings {
        sync_config: SyncConfig {
            transparent_address_discovery: TransparentAddressDiscovery {
                gap_limit: 25,
                scopes: TransparentAddressDiscoveryScopes {
                    external: true,
                    internal: true,
                    refund: false,
                },
            },
            performance_level: PerformanceLevel::Low,
            event_channel_capacity: 64,
        },
        min_confirmations: NonZeroU32::new(7).unwrap(),
    }
}

const PRICES_UPDATED: u32 = 1_700_000_000;
const CURRENT_PRICE: (u32, f32) = (1_700_086_400, 31.25);
const DAILY_PRICES: [(u32, f32); 3] = [
    (1_699_920_000, 29.5),
    (1_700_006_400, 30.75),
    (1_700_092_800, 31.0),
];

/// `PriceList` has no setters for its prices, so the list is decoded from its own record.
fn prices() -> PriceList {
    let price = |w: &mut Vec<u8>, (time, usd): (u32, f32)| {
        w.write_u32::<LittleEndian>(time)?;
        w.write_f32::<LittleEndian>(usd)
    };
    let mut record = vec![PriceList::VERSION];
    Optional::write(&mut record, Some(PRICES_UPDATED), |w, time| {
        w.write_u32::<LittleEndian>(time)
    })
    .unwrap();
    Optional::write(&mut record, Some(CURRENT_PRICE), price).unwrap();
    Vector::write(&mut record, &DAILY_PRICES, |w, daily| price(w, *daily)).unwrap();
    PriceList::read(record.as_slice(), ()).unwrap()
}

fn addresses_settings(chain_type: ChainType) -> WalletFile {
    let phrase = mnemonic(0x33);
    let keys = BTreeMap::from(
        [0, 1].map(|index| (account(index), spend_keys(chain_type, &phrase, index))),
    );
    let mut file = wallet(chain_type, Some(phrase), keys);
    file.unified_addresses.extend([
        (unified_id(0, 1), ReceiverSelection::sapling_only()),
        (unified_id(0, 2), ReceiverSelection::all_shielded()),
        (unified_id(1, 0), ReceiverSelection::all_shielded()),
        (unified_id(1, 1), ReceiverSelection::orchard_only()),
    ]);
    file.transparent_addresses.extend([
        transparent_id(0, TransparentScope::External, 1),
        transparent_id(0, TransparentScope::Internal, 0),
        transparent_id(0, TransparentScope::Internal, 1),
        transparent_id(0, TransparentScope::Refund, 0),
        transparent_id(0, TransparentScope::Refund, 1),
        transparent_id(1, TransparentScope::External, 0),
        transparent_id(1, TransparentScope::Internal, 0),
        transparent_id(1, TransparentScope::Refund, 0),
    ]);
    file.wallet_settings = settings();
    file.price_list = prices();
    file
}

fn sapling_note(
    recipient: sapling_crypto::PaymentAddress,
    value: u64,
    seed: u8,
) -> sapling_crypto::Note {
    sapling_crypto::Note::from_parts(
        recipient,
        sapling_crypto::value::NoteValue::from_raw(value),
        sapling_crypto::Rseed::AfterZip212([seed; 32]),
    )
}

fn orchard_note(
    recipient: orchard::Address,
    value: u64,
    seed: u8,
    version: NoteVersion,
) -> orchard::Note {
    let rho = Rho::from_bytes(&field_bytes(seed.into())).unwrap();
    let rseed = RandomSeed::from_bytes([seed; 32], &rho).unwrap();
    orchard::Note::from_parts(recipient, NoteValue::from_raw(value), rho, rseed, version).unwrap()
}

fn orchard_nullifier(seed: u8) -> orchard::note::Nullifier {
    orchard::note::Nullifier::from_bytes(&field_bytes(seed.into())).unwrap()
}

/// `OutgoingNote` has no public constructor, so outgoing notes are decoded from their own
/// record: version, output id, key id, recipient, value, rho, rseed, memo and the optional
/// recipient unified address.
fn outgoing_record(
    output_id: OutputId,
    note: &orchard::Note,
    memo: &Memo,
    recipient: Option<String>,
) -> Vec<u8> {
    let mut out = vec![1];
    output_id.write(&mut out).unwrap();
    KeyId {
        account_id: AccountId::ZERO,
        scope: Scope::External,
    }
    .write(&mut out)
    .unwrap();
    out.extend_from_slice(&note.recipient().to_raw_address_bytes());
    out.write_u64::<LittleEndian>(note.value().inner()).unwrap();
    out.extend_from_slice(&note.rho().to_bytes());
    out.extend_from_slice(note.rseed().as_bytes());
    out.extend_from_slice(memo.encode().as_array());
    Optional::write(&mut out, recipient, |w, address| {
        w.write_u64::<LittleEndian>(address.len() as u64)?;
        w.write_all(address.as_bytes())
    })
    .unwrap();
    out
}

fn text(memo: &str) -> Memo {
    memo.parse().unwrap()
}

/// One transaction per confirmation status, notes in every pool this release knows (Ironwood
/// only when `ironwood`), spent and unspent outputs, and populated nullifier, outpoint, sync
/// state and shard tree data.
fn notes(chain_type: ChainType, ironwood: bool) -> WalletFile {
    let keys = spend_keys(chain_type, &mnemonic(0x44), 0);
    let own = keys
        .generate_unified_address(0, ReceiverSelection::all_shielded())
        .unwrap();
    let orchard_to = *own.orchard().unwrap();
    let sapling_to = *own.sapling().unwrap();
    let coin_key = transparent_id(0, TransparentScope::External, 0);
    let coin_address = keys
        .generate_transparent_address(coin_key.address_index(), coin_key.scope())
        .unwrap();
    let coin = |output_id, value| {
        TransparentCoin::new_for_test(
            output_id,
            coin_key,
            encode_address(&chain_type, coin_address),
            TransparentAddress::script(&coin_address).into(),
            Zatoshis::from_u64(value).unwrap(),
        )
    };
    let orchard = |output_id, note, memo, position: Option<u64>| {
        OrchardNote::new_for_test(
            output_id,
            AccountId::ZERO,
            Scope::External,
            note,
            memo,
            position.map(Position::from),
        )
    };
    let sapling = |output_id, note, memo, position: Option<u64>| {
        SaplingNote::new_for_test(
            output_id,
            AccountId::ZERO,
            Scope::External,
            note,
            memo,
            position.map(Position::from),
        )
    };

    let [
        confirmed,
        mempool,
        transmitted,
        calculated,
        failed,
        ironwood_txid,
    ] = [1, 2, 3, 4, 5, 6].map(txid);

    let mut spent_orchard = orchard(
        OutputId::new(confirmed, 0),
        orchard_note(orchard_to, 100_000, 1, NoteVersion::V2),
        Memo::Empty,
        Some(0),
    )
    .with_nullifier_for_test(orchard_nullifier(1));
    spent_orchard.set_spending_transaction(Some(mempool));
    let kept_orchard = orchard(
        OutputId::new(confirmed, 1),
        orchard_note(orchard_to, 250_000, 2, NoteVersion::V2),
        text("orchard kept"),
        Some(1),
    )
    .with_nullifier_for_test(orchard_nullifier(2));
    let sent_note = orchard_note(orchard_to, 40_000, 3, NoteVersion::V2);
    let sent_orchard = OutgoingOrchardNote::read(
        outgoing_record(
            OutputId::new(confirmed, 2),
            &sent_note,
            &text("orchard sent"),
            None,
        )
        .as_slice(),
        &chain_type,
    )
    .unwrap();
    let mut spent_sapling = sapling(
        OutputId::new(confirmed, 0),
        sapling_note(sapling_to, 70_000, 4),
        Memo::Empty,
        Some(0),
    )
    .with_nullifier_for_test(sapling_crypto::Nullifier([4; 32]));
    spent_sapling.set_spending_transaction(Some(transmitted));
    let mut spent_coin = coin(OutputId::new(confirmed, 0), 30_000);
    spent_coin.set_spending_transaction(Some(calculated));

    let change_note = orchard_note(orchard_to, 60_000, 5, NoteVersion::V2);
    let sent_to_self = OutgoingOrchardNote::read(
        outgoing_record(
            OutputId::new(mempool, 1),
            &change_note,
            &text("orchard to self"),
            Some(own.encode(&chain_type)),
        )
        .as_slice(),
        &chain_type,
    )
    .unwrap();

    let mut transactions = vec![
        WalletTransaction::new_for_test_with_orchard_notes(
            confirmed,
            ConfirmationStatus::Confirmed(height(5)),
            vec![spent_orchard, kept_orchard],
            vec![sent_orchard],
        )
        .with_sapling_notes_for_test(vec![spent_sapling])
        .with_transparent_coins_for_test(vec![spent_coin]),
        WalletTransaction::new_for_test_with_orchard_notes(
            mempool,
            ConfirmationStatus::Mempool(height(21)),
            vec![orchard(
                OutputId::new(mempool, 0),
                orchard_note(orchard_to, 20_000, 6, NoteVersion::V2),
                Memo::Empty,
                None,
            )],
            vec![sent_to_self],
        ),
        WalletTransaction::new_for_test(transmitted, ConfirmationStatus::Transmitted(height(22)))
            .with_sapling_notes_for_test(vec![sapling(
                OutputId::new(transmitted, 0),
                sapling_note(sapling_to, 15_000, 7),
                text("sapling change"),
                None,
            )]),
        WalletTransaction::new_for_test(calculated, ConfirmationStatus::Calculated(height(23)))
            .with_transparent_coins_for_test(vec![coin(OutputId::new(calculated, 0), 5_000)]),
        WalletTransaction::new_for_test_with_orchard_notes(
            failed,
            ConfirmationStatus::Failed(height(24)),
            vec![orchard(
                OutputId::new(failed, 0),
                orchard_note(orchard_to, 9_000, 8, NoteVersion::V2),
                Memo::Empty,
                None,
            )],
            Vec::new(),
        ),
    ];
    if ironwood {
        let sent_note = orchard_note(orchard_to, 5_000, 10, NoteVersion::V3);
        transactions.push(WalletTransaction::new_for_test_with_ironwood_notes(
            ironwood_txid,
            ConfirmationStatus::Confirmed(height(6)),
            vec![
                IronwoodNote::new_for_test(
                    OutputId::new(ironwood_txid, 0),
                    AccountId::ZERO,
                    Scope::External,
                    orchard_note(orchard_to, 55_000, 9, NoteVersion::V3),
                    text("ironwood kept"),
                    Some(Position::from(0)),
                )
                .with_nullifier_for_test(orchard_nullifier(9)),
            ],
            vec![
                OutgoingIronwoodNote::read(
                    outgoing_record(
                        OutputId::new(ironwood_txid, 1),
                        &sent_note,
                        &Memo::Empty,
                        None,
                    )
                    .as_slice(),
                    &chain_type,
                )
                .unwrap(),
            ],
        ));
    }

    let mut file = wallet(
        chain_type,
        Some(mnemonic(0x44)),
        BTreeMap::from([(AccountId::ZERO, keys)]),
    );
    file.wallet_transactions = transactions
        .into_iter()
        .map(|transaction| (transaction.txid(), transaction))
        .collect();

    let target = |offset, txid, narrow_scan_area| ScanTarget {
        block_height: height(offset),
        txid,
        narrow_scan_area,
    };
    file.nullifier_map.sapling.insert(
        sapling_crypto::Nullifier([4; 32]),
        target(22, transmitted, false),
    );
    file.nullifier_map
        .orchard
        .insert(orchard_nullifier(1), target(21, mempool, false));
    if ironwood {
        file.nullifier_map
            .ironwood
            .insert(orchard_nullifier(11), target(6, ironwood_txid, false));
    }
    file.outpoint_map
        .insert(OutputId::new(confirmed, 0), target(23, calculated, true));

    file.sync_state = SyncState::new_for_test(
        [
            ScanPriority::Scanned,
            ScanPriority::ScannedWithoutMapping,
            ScanPriority::RefetchingNullifiers,
            ScanPriority::Scanning,
            ScanPriority::Historic,
            ScanPriority::OpenAdjacent,
            ScanPriority::FoundNote,
            ScanPriority::Verify,
            ScanPriority::ChainTip,
        ]
        .into_iter()
        .zip(0..)
        .map(|(priority, step)| {
            ScanRange::from_parts(height(step * 10)..height(step * 10 + 10), priority)
        })
        .collect(),
    );
    add_scan_targets(
        &mut file.sync_state,
        &[
            target(45, txid(7), true),
            target(65, TxId::from_bytes([0; 32]), false),
        ],
    );

    for leaf in 0..TREE_LEAVES {
        let retention = Retention::Checkpoint {
            id: height(leaf),
            marking: if leaf < 2 {
                Marking::Marked
            } else {
                Marking::None
            },
        };
        let bytes = field_bytes(leaf + 1);
        file.shard_trees
            .sapling
            .append(sapling_crypto::Node::from_bytes(bytes).unwrap(), retention)
            .unwrap();
        file.shard_trees
            .orchard
            .append(MerkleHashOrchard::from_bytes(&bytes).unwrap(), retention)
            .unwrap();
        if ironwood {
            file.shard_trees
                .ironwood
                .append(MerkleHashOrchard::from_bytes(&bytes).unwrap(), retention)
                .unwrap();
        }
    }
    file
}

fn assert_first_addresses(name: &str, file: &WalletFile) {
    assert_eq!(file.birthday, height(0), "{name}");
    assert_eq!(
        file.unified_addresses,
        BTreeMap::from([(unified_id(0, 0), ReceiverSelection::orchard_only())]),
        "{name}"
    );
    assert_eq!(
        file.transparent_addresses,
        BTreeSet::from([transparent_id(0, TransparentScope::External, 0)]),
        "{name}"
    );
    assert!(file.wallet_transactions.is_empty(), "{name}");
    assert_eq!(file.wallet_settings, WalletSettings::default(), "{name}");
    assert!(file.price_list.current_price().is_none(), "{name}");
}

fn assert_empty(decoded: Vec<(&str, WalletFile)>) {
    for (name, file) in decoded {
        assert_eq!(
            file.mnemonic.as_ref().map(Mnemonic::phrase),
            Some(mnemonic(0x11).phrase()),
            "{name}"
        );
        assert_eq!(file.unified_key_store.len(), 1, "{name}");
        assert!(
            file.unified_key_store[&AccountId::ZERO].is_spending_key(),
            "{name}"
        );
        assert_first_addresses(name, &file);
    }
}

fn assert_view(decoded: Vec<(&str, WalletFile)>) {
    for (name, file) in decoded {
        assert!(file.mnemonic.is_none(), "{name}");
        assert_eq!(file.unified_key_store.len(), 1, "{name}");
        assert!(
            matches!(
                file.unified_key_store[&AccountId::ZERO],
                UnifiedKeyStore::View(_)
            ),
            "{name}"
        );
        assert_first_addresses(name, &file);
    }
}

fn assert_addresses_settings(decoded: Vec<(&str, WalletFile)>) {
    for (name, file) in decoded {
        assert_eq!(file.birthday, height(0), "{name}");
        assert_eq!(
            file.unified_key_store.keys().copied().collect::<Vec<_>>(),
            [account(0), account(1)],
            "{name}"
        );
        assert_eq!(file.unified_addresses.len(), 5, "{name}");
        assert_eq!(
            file.unified_addresses[&unified_id(0, 1)],
            ReceiverSelection::sapling_only(),
            "{name}"
        );
        assert_eq!(
            file.unified_addresses[&unified_id(1, 0)],
            ReceiverSelection::all_shielded(),
            "{name}"
        );
        assert_eq!(file.transparent_addresses.len(), 9, "{name}");
        for scope in [
            TransparentScope::External,
            TransparentScope::Internal,
            TransparentScope::Refund,
        ] {
            assert_eq!(
                file.transparent_addresses
                    .iter()
                    .filter(|id| id.scope() == scope)
                    .count(),
                3,
                "{name}: {scope} addresses"
            );
        }
        assert_eq!(file.wallet_settings, settings(), "{name}");
        let current = file.price_list.current_price().unwrap();
        assert_eq!((current.time, current.price_usd), CURRENT_PRICE, "{name}");
        assert_eq!(
            file.price_list
                .daily_prices()
                .iter()
                .map(|price| (price.time, price.price_usd))
                .collect::<Vec<_>>(),
            DAILY_PRICES,
            "{name}"
        );
        assert_eq!(
            file.price_list.time_historical_prices_last_updated(),
            Some(PRICES_UPDATED),
            "{name}"
        );
    }
}

fn assert_notes(decoded: Vec<(&str, WalletFile)>, ironwood: bool) {
    for (name, file) in decoded {
        let mut statuses = BTreeMap::from([
            (txid(1), ConfirmationStatus::Confirmed(height(5))),
            (txid(2), ConfirmationStatus::Mempool(height(21))),
            (txid(3), ConfirmationStatus::Transmitted(height(22))),
            (txid(4), ConfirmationStatus::Calculated(height(23))),
            (txid(5), ConfirmationStatus::Failed(height(24))),
        ]);
        if ironwood {
            statuses.insert(txid(6), ConfirmationStatus::Confirmed(height(6)));
        }
        assert_eq!(
            file.wallet_transactions
                .iter()
                .map(|(txid, transaction)| (*txid, transaction.status()))
                .collect::<BTreeMap<_, _>>(),
            statuses,
            "{name}"
        );

        let confirmed = &file.wallet_transactions[&txid(1)];
        assert_eq!(confirmed.orchard_notes().len(), 2, "{name}");
        assert_eq!(
            confirmed.orchard_notes()[0].spending_transaction(),
            Some(txid(2)),
            "{name}"
        );
        assert_eq!(
            confirmed.orchard_notes()[1].spending_transaction(),
            None,
            "{name}"
        );
        assert_eq!(confirmed.outgoing_orchard_notes().len(), 1, "{name}");
        assert_eq!(
            confirmed.sapling_notes()[0].spending_transaction(),
            Some(txid(3)),
            "{name}"
        );
        assert_eq!(
            confirmed.transparent_coins()[0].spending_transaction(),
            Some(txid(4)),
            "{name}"
        );
        assert_eq!(confirmed.total_value_received(), 450_000, "{name}");
        assert_eq!(
            file.wallet_transactions
                .values()
                .map(|transaction| transaction.ironwood_notes().len())
                .sum::<usize>(),
            usize::from(ironwood),
            "{name}"
        );

        assert_eq!(file.nullifier_map.sapling.len(), 1, "{name}");
        assert_eq!(file.nullifier_map.orchard.len(), 1, "{name}");
        assert_eq!(
            file.nullifier_map.ironwood.len(),
            usize::from(ironwood),
            "{name}"
        );
        assert_eq!(file.outpoint_map.len(), 1, "{name}");

        let scan_ranges = file.sync_state.scan_ranges();
        assert_eq!(scan_ranges.len(), 9, "{name}");
        assert_eq!(
            scan_ranges
                .iter()
                .map(ScanRange::priority)
                .collect::<BTreeSet<_>>()
                .len(),
            9,
            "{name}: one range per scan priority"
        );
        assert_eq!(file.sync_state.scan_targets().len(), 2, "{name}");

        assert_eq!(
            file.shard_trees.sapling.store().checkpoint_count().unwrap(),
            100,
            "{name}"
        );
        assert_eq!(
            file.shard_trees.orchard.store().checkpoint_count().unwrap(),
            100,
            "{name}"
        );
        assert_eq!(
            file.shard_trees
                .ironwood
                .store()
                .checkpoint_count()
                .unwrap(),
            if ironwood { 100 } else { 1 },
            "{name}"
        );
    }
}

#[test]
fn v0_0_1_empty() {
    assert_empty(check(
        Release::V001,
        vectors!("pendrake-v0.0.1", "empty"),
        empty,
    ));
}

#[test]
fn v0_0_1_view() {
    assert_view(check(
        Release::V001,
        vectors!("pendrake-v0.0.1", "view"),
        view,
    ));
}

#[test]
fn v0_0_1_addresses_settings() {
    assert_addresses_settings(check(
        Release::V001,
        vectors!("pendrake-v0.0.1", "addresses-settings"),
        addresses_settings,
    ));
}

#[test]
fn v0_0_1_notes() {
    assert_notes(
        check(
            Release::V001,
            vectors!("pendrake-v0.0.1", "notes"),
            |chain_type| notes(chain_type, false),
        ),
        false,
    );
}

#[test]
fn v0_0_2_empty() {
    assert_empty(check(
        Release::V002,
        vectors!("pendrake-v0.0.2", "empty"),
        empty,
    ));
}

#[test]
fn v0_0_2_view() {
    assert_view(check(
        Release::V002,
        vectors!("pendrake-v0.0.2", "view"),
        view,
    ));
}

#[test]
fn v0_0_2_addresses_settings() {
    assert_addresses_settings(check(
        Release::V002,
        vectors!("pendrake-v0.0.2", "addresses-settings"),
        addresses_settings,
    ));
}

#[test]
fn v0_0_2_notes() {
    assert_notes(
        check(
            Release::V002,
            vectors!("pendrake-v0.0.2", "notes"),
            |chain_type| notes(chain_type, true),
        ),
        true,
    );
}
