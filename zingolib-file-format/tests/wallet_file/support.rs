//! Shared helpers for the `zingolib-file-format` integration tests: building a fresh wallet
//! deterministically, serializing it, and running a corrupted-byte read sweep against it.
//!
//! Shared by every module of the `wallet_file` test binary
//! by every binary.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::panic;
use std::sync::mpsc;
use std::time::Duration;

use bip0039::Mnemonic;
use zcash_protocol::consensus::BlockHeight;
use zcash_transparent::keys::NonHardenedChildIndex;
use zip32::AccountId;

use pepper_sync::keys::transparent::TransparentAddressId;
use pepper_sync::wallet::{NullifierMap, ShardTrees, SyncState};
use zingolib_common::chain::ChainType;
use zingolib_common::keys::{TransparentScope, UnifiedAddressId, UnifiedKeyStore};
use zingolib_price::PriceList;

use zingolib_file_format::encryption::{Argon2Params, EncryptionConfig, EncryptionSession};
use zingolib_file_format::{WalletFile, WalletFileRef, WalletSettings};

/// How long a single mutated read is allowed to run before it counts as a hang.
const READ_TIMEOUT: Duration = Duration::from_secs(3);

/// How many byte offsets a sweep visits. A read costs milliseconds, so a sweep over every
/// offset stays within seconds only for small inputs; larger ones take a fixed number of
/// offsets spread evenly from the first byte to the last.
#[derive(Clone, Copy)]
pub enum Coverage {
    Exhaustive,
    Sampled(usize),
}

/// The maximal `CompactSize` encodings, tried at every sampled offset so a length prefix a
/// reader trusts blindly turns into a huge allocation or loop count instead of an error.
pub const COMPACT_SIZE_MAX_ENCODINGS: [&[u8]; 3] = [
    &[0xFD, 0xFF, 0xFF],
    &[0xFE, 0xFF, 0xFF, 0xFF, 0xFF],
    &[0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
];

/// Argon2 parameters cheap enough for a test suite: same shape as production, minimal cost.
pub fn fast_session(passphrase: &str) -> EncryptionSession {
    EncryptionConfig::with_params(
        passphrase.to_string(),
        Argon2Params {
            m_cost: 8,
            t_cost: 1,
            p_cost: 1,
        },
    )
    .derive()
    .unwrap()
}

/// A deterministic mnemonic from fixed entropy, keyed by `seed` so callers can build distinct
/// wallets without depending on randomness.
pub fn mnemonic(seed: u8) -> Mnemonic {
    Mnemonic::from_entropy([seed; 32]).unwrap()
}

/// A minimal, valid wallet: one spending account, its default receivers and first external
/// transparent address.
pub fn fresh(chain_type: ChainType, seed: u8) -> WalletFile {
    let phrase = mnemonic(seed);
    let keys = UnifiedKeyStore::new_from_mnemonic(chain_type, &phrase, AccountId::ZERO).unwrap();
    let mut unified_addresses = BTreeMap::new();
    unified_addresses.insert(
        UnifiedAddressId {
            account_id: AccountId::ZERO,
            address_index: 0,
        },
        keys.default_receivers().unwrap(),
    );
    let transparent_addresses = BTreeSet::from([TransparentAddressId::new(
        AccountId::ZERO,
        TransparentScope::External,
        NonHardenedChildIndex::ZERO,
    )]);
    WalletFile {
        read_version: WalletFile::VERSION,
        chain_type,
        mnemonic: Some(phrase),
        birthday: BlockHeight::from_u32(2_000_000),
        unified_key_store: BTreeMap::from([(AccountId::ZERO, keys)]),
        unified_addresses,
        transparent_addresses,
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

/// Serializes `file` the way it would be saved to disk.
pub fn bytes(file: &WalletFile) -> Vec<u8> {
    let mut out = Vec::new();
    WalletFileRef::from(file)
        .write(&mut out, &file.chain_type)
        .unwrap();
    out
}

/// Runs every byte-level mutation against `bytes` and asserts that the reader survives it.
///
/// `label` names the input in failure messages, e.g. "mainnet plain" or a vector file path.
/// `passphrase` picks `WalletFile::read_encrypted_any` (`Some`) over `WalletFile::read_any`
/// (`None`).
///
/// A truncated file must always be rejected: some sequentially-read field is left short, and
/// nothing in the format tolerates that. A bit flip or an oversized length prefix is different:
/// several fields have no content-level validation at all by design (raw seed entropy is any
/// byte string of a valid length; unused bits in a flags byte are silently ignored; a hash or
/// txid is passed through opaquely), so mutating one can legitimately decode into a different,
/// still-well-formed wallet rather than an error. Asserting "always rejected" for those would be
/// asserting something false about the format, not testing hardening. So this sweep asserts the
/// property that mutation is actually about: the reader never panics and never hangs, on any
/// input. Where rejection *is* guaranteed (truncation), it is asserted too.
pub fn mutation_sweep(label: &str, bytes: &[u8], passphrase: Option<&str>, coverage: Coverage) {
    let len = bytes.len();
    let positions = sample_positions(len, coverage);

    for &offset in &positions {
        let mut mutated = bytes.to_vec();
        mutated[offset] ^= 0xFF;
        assert_survives(label, "bit flip", offset, &mutated, passphrase);
    }

    for &offset in &positions {
        assert_rejected(label, "truncation", offset, &bytes[..offset], passphrase);
    }

    for &offset in &positions {
        for pattern in COMPACT_SIZE_MAX_ENCODINGS {
            if offset + pattern.len() > len {
                continue;
            }
            let mut mutated = bytes.to_vec();
            mutated[offset..offset + pattern.len()].copy_from_slice(pattern);
            assert_survives(
                label,
                "compact-size max length",
                offset,
                &mutated,
                passphrase,
            );
        }
    }
}

/// The byte offsets a sweep visits under `coverage`.
fn sample_positions(len: usize, coverage: Coverage) -> Vec<usize> {
    let samples = match coverage {
        Coverage::Exhaustive => return (0..len).collect(),
        Coverage::Sampled(samples) if samples >= len => return (0..len).collect(),
        Coverage::Sampled(samples) => samples,
    };
    (0..samples)
        .map(|i| i * (len - 1) / (samples - 1))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Checks that reading `bytes` neither panics nor hangs. Whether it is accepted or rejected is
/// not checked: see the [`mutation_sweep`] doc for why that can't be a blanket requirement.
pub fn read_survives(bytes: &[u8], passphrase: Option<&str>) -> Result<(), String> {
    match read_outcome(bytes, passphrase) {
        ReadOutcome::Rejected | ReadOutcome::Accepted => Ok(()),
        ReadOutcome::Panicked(message) => {
            Err(format!("panicked instead of returning an error: {message}"))
        }
        ReadOutcome::TimedOut => Err(format!("did not return within {READ_TIMEOUT:?}")),
    }
}

fn assert_survives(label: &str, kind: &str, offset: usize, bytes: &[u8], passphrase: Option<&str>) {
    if let Err(failure) = read_survives(bytes, passphrase) {
        panic!("{label}: {kind} at offset {offset} {failure}");
    }
}

/// Asserts the mutated read is rejected, and neither panics nor hangs. Used only for mutation
/// kinds that are guaranteed to desync the reader, such as truncation.
fn assert_rejected(label: &str, kind: &str, offset: usize, bytes: &[u8], passphrase: Option<&str>) {
    match read_outcome(bytes, passphrase) {
        ReadOutcome::Rejected => {}
        ReadOutcome::Accepted => panic!(
            "{label}: {kind} at offset {offset} was accepted; expected `WalletFile::read_any` \
             (or `read_encrypted_any`) to return an error"
        ),
        ReadOutcome::Panicked(message) => panic!(
            "{label}: {kind} at offset {offset} panicked instead of returning an error: {message}"
        ),
        ReadOutcome::TimedOut => {
            panic!("{label}: {kind} at offset {offset} did not return within {READ_TIMEOUT:?}")
        }
    }
}

enum ReadOutcome {
    Accepted,
    Rejected,
    Panicked(String),
    TimedOut,
}

/// Runs the read in a spawned thread so a panic is caught and a hang shows up as a timeout
/// instead of stalling the whole sweep.
fn read_outcome(bytes: &[u8], passphrase: Option<&str>) -> ReadOutcome {
    let bytes = bytes.to_vec();
    let passphrase = passphrase.map(str::to_string);
    let (tx, rx) = mpsc::channel();

    std::thread::spawn(move || {
        let is_err = panic::catch_unwind(move || match &passphrase {
            Some(passphrase) => {
                WalletFile::read_encrypted_any(bytes.as_slice(), Some(passphrase.clone())).is_err()
            }
            None => WalletFile::read_any(bytes.as_slice()).is_err(),
        });
        let _ = tx.send(is_err);
    });

    match rx.recv_timeout(READ_TIMEOUT) {
        Ok(Ok(true)) => ReadOutcome::Rejected,
        Ok(Ok(false)) => ReadOutcome::Accepted,
        Ok(Err(payload)) => ReadOutcome::Panicked(panic_message(payload)),
        Err(_) => ReadOutcome::TimedOut,
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        message.to_string()
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else {
        "non-string panic payload".to_string()
    }
}
