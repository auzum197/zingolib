//! Core module, containing `crate::wallet::LightWallet` with methods for all wallet functionality.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use tokio::sync::watch;

use bip0039::Mnemonic;

use zcash_keys::address::UnifiedAddress;
use zcash_primitives::transaction::TxId;
use zcash_protocol::consensus::{BlockHeight, Parameters};

use ekdromos_common::price::PriceList;
use pepper_sync::wallet::{ScanTarget, ShardTrees};
use pepper_sync::{
    keys::transparent::TransparentAddressId,
    wallet::{NullifierMap, OutputId, SyncState, WalletBlock, WalletTransaction},
};

use crate::config::{ChainType, WalletConfig};
use error::{PriceError, WalletError};
use keys::unified::{ReceiverSelection, UnifiedAddressId, UnifiedKeyStore};
use locks::OutputLocks;
use pepper_sync::keys::transparent::TransparentScope;
use zcash_transparent::keys::NonHardenedChildIndex;

pub mod error;
pub mod utils;

// these mods contain pieces of the impl LightWallet
pub mod balance;
pub mod disk;
pub mod keys;
mod locks;
pub mod output;
pub mod spendable;
pub mod summary;
pub mod sync;
pub mod transaction;
mod zcb_traits;

pub use ekdromos_file_format::{WalletSettings, encryption};
pub use pepper_sync::config::{
    PerformanceLevel, SyncConfig, TransparentAddressDiscovery, TransparentAddressDiscoveryScopes,
};

/// The address set every wallet starts with: the default unified address of account 0
/// and its first external transparent address. Address derivation later drops the
/// transparent entry when the keys cannot view transparent funds.
fn first_addresses(
    unified_key: &UnifiedKeyStore,
) -> (
    BTreeMap<UnifiedAddressId, ReceiverSelection>,
    BTreeSet<TransparentAddressId>,
) {
    let mut unified_addresses = BTreeMap::new();
    if let Some(receivers) = unified_key.default_receivers() {
        unified_addresses.insert(
            UnifiedAddressId {
                account_id: zip32::AccountId::ZERO,
                address_index: 0,
            },
            receivers,
        );
    }
    let transparent_addresses = BTreeSet::from([TransparentAddressId::new(
        zip32::AccountId::ZERO,
        TransparentScope::External,
        NonHardenedChildIndex::ZERO,
    )]);
    (unified_addresses, transparent_addresses)
}

/// Provides necessary information to recover the wallet without the wallet file.
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct RecoveryInfo {
    /// 24-word mnemonic phrase.
    pub seed_phrase: String,
    /// Block height wallet was created.
    pub birthday: u64,
    /// Number of accounts in use.
    pub no_of_accounts: u32,
}

impl std::fmt::Display for RecoveryInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Wallet backup info:
{{
    seed phrase: {}
    birthday: {}
    no_of_accounts: {}
}}",
            self.seed_phrase, self.birthday, self.no_of_accounts,
        )
    }
}

/// Base data required to construct a new [`crate::wallet::LightWallet`].
pub(crate) struct WalletBase {
    pub(crate) unified_key_store: BTreeMap<zip32::AccountId, UnifiedKeyStore>,
    pub(crate) mnemonic: Option<Mnemonic>,
    pub(crate) birthday: BlockHeight,
    pub(crate) wallet_settings: WalletSettings,
    /// Optional at-rest encryption. Set by [`crate::wallet::LightWallet::new`] (not by
    /// `resolve`). The derived session is produced in `from_base`. Holds a secret, so
    /// `WalletBase` intentionally has no `Debug`/`Clone`/`Eq` derives.
    pub(crate) encryption: Option<encryption::EncryptionConfig>,
}

/// In-memory wallet data struct
///
/// The `mnemonic` can be `None` in the case of a wallet created directly from UFVKs or USKs.
///
/// As no relevant transactions related to this wallet will exist below the wallet's birthday, sync will start from
/// `birthday` block height.
///
/// Every change to wallet state due to sync, send or creating addresses is counted.
/// [`crate::wallet::LightWallet::save`] returns the bytes to be persisted when the count has moved since the last
/// save. A [`crate::lightclient::LightClient`] saves its wallet this way without being asked.
#[derive(Debug)]
pub struct LightWallet {
    current_version: u64,
    /// Wallet version that was read from on wallet load.
    read_version: u64,
    /// Blockchain network type
    chain_type: ChainType,
    /// The seed for the wallet, stored as a zip339 Mnemonic.
    mnemonic: Option<Mnemonic>,
    /// The block height at which the wallet was created.
    birthday: BlockHeight,
    pub unified_key_store: BTreeMap<zip32::AccountId, UnifiedKeyStore>,
    unified_addresses: BTreeMap<UnifiedAddressId, UnifiedAddress>,
    transparent_addresses: BTreeMap<TransparentAddressId, String>,
    pub wallet_blocks: BTreeMap<BlockHeight, WalletBlock>,
    pub wallet_transactions: HashMap<TxId, WalletTransaction>,
    pub nullifier_map: NullifierMap,
    pub outpoint_map: BTreeMap<OutputId, ScanTarget>,
    pub shard_trees: ShardTrees,
    pub sync_state: SyncState,
    pub wallet_settings: WalletSettings,
    /// The current and historical daily price of zec.
    pub price_list: PriceList,
    /// The proposal the test harness is about to send. The wallet is watch-only, so this
    /// exists only for the tests.
    #[cfg(any(test, feature = "testutils"))]
    pub(crate) send_proposal: Option<crate::testutils::send::proposal::StoredProposal>,
    /// Advisory output locks held by in-flight proposals. Not persisted.
    output_locks: OutputLocks,
    /// Counts changes to the wallet state.
    changes: watch::Sender<u64>,
    /// The value of `changes` at the last save.
    saved: u64,
    /// At-rest encryption session. When `Some`, the serialized wallet is wrapped in a
    /// passphrase-derived AEAD envelope on every save (see [`crate::wallet::encryption`]).
    /// `None` means the wallet is persisted in the clear (legacy / opt-out). The expensive
    /// key derivation is performed once when this is set. Saves only run the cheap AEAD step.
    encryption: Option<encryption::EncryptionSession>,
}

impl LightWallet {
    /// Create a new in-memory wallet from [`crate::config::WalletConfig`], optionally encrypting
    /// it at rest with the given [`crate::wallet::encryption::EncryptionConfig`] (so the wallet
    /// is encrypted from its first save).
    #[allow(clippy::result_large_err)]
    pub fn new(
        chain_type: ChainType,
        wallet_config: WalletConfig,
        encryption: Option<encryption::EncryptionConfig>,
    ) -> Result<Self, WalletError> {
        let mut wallet_base = wallet_config.resolve(chain_type)?;
        wallet_base.encryption = encryption;
        Self::from_base(chain_type, wallet_base)
    }

    /// Construct a wallet from [`crate::wallet::WalletBase`], resolved from a [`crate::config::WalletConfig`].
    #[allow(clippy::result_large_err)]
    pub(crate) fn from_base(
        chain_type: ChainType,
        wallet_base: WalletBase,
    ) -> Result<Self, WalletError> {
        let WalletBase {
            unified_key_store,
            mnemonic,
            birthday,
            wallet_settings,
            encryption,
        } = wallet_base;

        let sapling_activation_height = chain_type
            .activation_height(zcash_protocol::consensus::NetworkUpgrade::Sapling)
            .expect("should have some sapling activation height");
        if birthday < sapling_activation_height {
            return Err(WalletError::BirthdayBelowSapling(
                u32::from(birthday),
                u32::from(sapling_activation_height),
            ));
        }

        let (unified_addresses, transparent_addresses) = first_addresses(
            unified_key_store
                .get(&zip32::AccountId::ZERO)
                .expect("account 0 must exist"),
        );

        // Derive the at-rest encryption key now (once), if requested, so the first save is
        // already encrypted.
        let encryption = encryption
            .map(encryption::EncryptionConfig::derive)
            .transpose()?;

        let mut wallet = Self::from_file(disk::WalletFile {
            read_version: Self::serialized_version(),
            chain_type,
            mnemonic,
            birthday: BlockHeight::from_u32(birthday.into()),
            unified_key_store,
            unified_addresses,
            transparent_addresses,
            wallet_blocks: BTreeMap::new(),
            wallet_transactions: HashMap::new(),
            nullifier_map: NullifierMap::new(),
            outpoint_map: BTreeMap::new(),
            shard_trees: ShardTrees::new(),
            sync_state: SyncState::new(),
            wallet_settings,
            price_list: PriceList::new(),
        })?;
        wallet.mark_dirty();
        wallet.encryption = encryption;

        Ok(wallet)
    }

    #[must_use]
    pub fn current_version(&self) -> u64 {
        self.current_version
    }

    /// Returns wallet version that was read from on wallet load.
    #[must_use]
    pub fn read_version(&self) -> u64 {
        self.read_version
    }

    #[must_use]
    pub fn birthday(&self) -> BlockHeight {
        self.birthday
    }

    #[must_use]
    pub fn chain_type(&self) -> ChainType {
        self.chain_type
    }

    /// Returns the wallet's mnemonic for internal operations.
    #[must_use]
    pub(crate) fn mnemonic(&self) -> Option<&Mnemonic> {
        self.mnemonic.as_ref()
    }

    #[must_use]
    pub fn mnemonic_phrase(&self) -> Option<String> {
        self.mnemonic().map(|m| m.phrase().to_string())
    }

    #[must_use]
    pub fn unified_addresses(&self) -> &BTreeMap<UnifiedAddressId, UnifiedAddress> {
        &self.unified_addresses
    }

    #[must_use]
    pub fn transparent_addresses(&self) -> &BTreeMap<TransparentAddressId, String> {
        &self.transparent_addresses
    }

    #[must_use]
    pub fn recovery_info(&self) -> Option<RecoveryInfo> {
        Some(RecoveryInfo {
            seed_phrase: self.mnemonic_phrase()?,
            birthday: self.birthday.into(),
            no_of_accounts: self.unified_key_store.len() as u32,
        })
    }

    #[allow(clippy::result_large_err)]
    pub fn create_new_account(&mut self) -> Result<(), WalletError> {
        let last_account = self.unified_key_store.keys().copied().max();
        let account_id = last_account.map_or(Ok(zip32::AccountId::ZERO), |last_account| {
            last_account
                .next()
                .ok_or(WalletError::AccountCreationFailed)
        })?;
        self.unified_key_store.insert(
            account_id,
            UnifiedKeyStore::new_from_mnemonic(
                self.chain_type(),
                self.mnemonic().ok_or(WalletError::MnemonicNotFound)?,
                account_id,
            )?,
        );

        Ok(())
    }

    /// Records a change to the wallet state, so the next [`Self::save`] returns bytes.
    pub fn mark_dirty(&mut self) {
        self.changes.send_modify(|count| *count += 1);
    }

    /// The change count. It moves on every change to the wallet state.
    pub fn subscribe_changes(&self) -> watch::Receiver<u64> {
        self.changes.subscribe()
    }

    /// If the wallet state has changed since last save, serializes the wallet and returns the wallet bytes.
    /// Returns `Ok(None)` if the wallet state has not changed and save is not required.
    /// Returns error if serialization fails.
    pub fn save(&mut self) -> std::io::Result<Option<Vec<u8>>> {
        let changes = *self.changes.borrow();
        if changes != self.saved {
            let chain_type = self.chain_type;
            // Serialize into a buffer that is zeroized on drop, since while a passphrase is
            // set this plaintext (containing the seed and spending keys) must never touch
            // disk and should not linger in freed memory longer than necessary.
            let mut wallet_bytes: zeroize::Zeroizing<Vec<u8>> = zeroize::Zeroizing::new(vec![]);
            self.write(&mut *wallet_bytes, &chain_type)?;
            let out = match &self.encryption {
                Some(session) => session
                    .encrypt(&wallet_bytes)
                    .map_err(|e| std::io::Error::other(e.to_string()))?,
                None => wallet_bytes.to_vec(),
            };
            // Only record the save once the bytes are successfully produced. If serialization
            // or encryption above fails, the save stays due so it is retried rather than
            // silently dropped.
            self.saved = changes;
            Ok(Some(out))
        } else {
            Ok(None)
        }
    }

    /// Returns `true` if this wallet is configured to encrypt its file at rest.
    #[must_use]
    pub fn is_encrypted(&self) -> bool {
        self.encryption.is_some()
    }

    /// Enable at-rest encryption on a previously unencrypted wallet, or replace the current
    /// passphrase with a new one, using the default
    /// ([`encryption::Argon2Params::default`]) KDF parameters.
    ///
    /// Memory-constrained callers (e.g. mobile) should prefer
    /// [`Self::set_passphrase_with_params`] with
    /// [`encryption::Argon2Params::with_memory_mib`] and a smaller memory cost.
    pub fn set_passphrase(
        &mut self,
        passphrase: String,
    ) -> Result<(), encryption::WalletEncryptionError> {
        self.set_passphrase_with_params(passphrase, encryption::Argon2Params::default())
    }

    /// Enable at-rest encryption (or rotate the passphrase) with explicit Argon2id parameters.
    ///
    /// Runs the (intentionally slow) key derivation once and caches the resulting key.
    /// Subsequent saves only run the fast AEAD step. A fresh random salt is generated, so
    /// calling this to rotate a passphrase fully rekeys the file. Marks the wallet dirty so the
    /// change is persisted on the next save.
    pub fn set_passphrase_with_params(
        &mut self,
        passphrase: String,
        params: encryption::Argon2Params,
    ) -> Result<(), encryption::WalletEncryptionError> {
        self.encryption =
            Some(encryption::EncryptionConfig::with_params(passphrase, params).derive()?);
        self.mark_dirty();
        Ok(())
    }

    /// Rotate the passphrase. Alias for [`Self::set_passphrase`], named for intent at the
    /// call site. Both fully rekey the file with a new salt.
    pub fn change_passphrase(
        &mut self,
        passphrase: String,
    ) -> Result<(), encryption::WalletEncryptionError> {
        self.set_passphrase(passphrase)
    }

    /// Disable at-rest encryption: the next save (and all subsequent saves) will write the
    /// wallet in the clear. This is an explicit opt-out, so use with care.
    pub fn remove_passphrase(&mut self) {
        if self.encryption.take().is_some() {
            self.mark_dirty();
        }
    }

    /// Update and return current price of ZEC.
    ///
    /// Currently only USD is supported.
    pub async fn update_current_price(&mut self) -> Result<f32, PriceError> {
        let current_price = crate::price::get_current_price().await?.price_usd;
        self.mark_dirty();

        Ok(current_price)
    }

    /// Prunes historical prices to days containing transactions in the wallet.
    ///
    /// Avoids pruning above fully scanned height.
    // TODO: under development
    pub fn prune_price_list(&mut self) {
        let Some(fully_scanned_height) = self.sync_state.fully_scanned_height() else {
            return;
        };
        let transaction_times = self
            .wallet_transactions
            .values()
            .filter(|transaction| {
                transaction
                    .status()
                    .get_confirmed_height()
                    .is_some_and(|height| height <= fully_scanned_height)
            })
            .map(pepper_sync::wallet::WalletTransaction::datetime)
            .collect();

        let prune_below = self
            .wallet_blocks
            .get(&fully_scanned_height)
            .expect("fully scanned height should always be on a scan range boundary")
            .time();
        self.price_list.prune(transaction_times, prune_below);
    }

    /// Clears all wallet data obtained from the block chain including the sync state.
    ///
    /// Adds scan targets to the new sync state to prioritise scanning relevant parts of the chain on rescan.
    /// Addresses are not cleared.
    pub fn clear_all(&mut self) {
        self.sync_state = SyncState::new();
        pepper_sync::add_scan_targets(
            &mut self.sync_state,
            &self
                .wallet_transactions
                .values()
                .filter_map(|transaction| {
                    transaction
                        .status()
                        .get_confirmed_height()
                        .map(|height| ScanTarget {
                            block_height: height,
                            txid: transaction.txid(),
                            narrow_scan_area: true,
                        })
                })
                .collect::<Vec<_>>(),
        );

        self.wallet_blocks.clear();
        self.wallet_transactions.clear();
        self.nullifier_map.clear();
        self.outpoint_map.clear();
        self.shard_trees = ShardTrees::new();
        self.price_list = PriceList::new();

        self.mark_dirty();
    }
}
