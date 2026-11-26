use std::{
    collections::BTreeMap,
    sync::{Arc, atomic::AtomicBool},
    time::Duration,
};

use tokio::{
    sync::{RwLock, watch},
    task::JoinHandle,
};

use bip0039::Mnemonic;
use zcash_keys::address::UnifiedAddress;
use zcash_primitives::transaction::TxId;
use zcash_protocol::consensus::BlockHeight;
use zcash_transparent::address::TransparentAddress;

use pepper_sync::{
    error::SyncError, events::SyncEmitter, keys::transparent::TransparentAddressId,
    sync::SyncResult, wallet::SyncMode,
};
use zingo_netutils::{Indexer as _, lightwallet_protocol::LightdInfo};

use crate::{
    config::{ChainType, WalletConfig},
    wallet::{
        LightWallet,
        balance::AccountBalance,
        encryption::{self, EncryptionConfig, WalletEncryptionError},
        error::{BalanceError, KeyError, SummaryError, WalletError},
        keys::unified::{ReceiverSelection, UnifiedAddressId},
        summary::data::{
            TransactionSummaries, TransactionSummary, WalletEvents,
            finsight::{TotalMemoBytesToAddress, TotalSendsToAddress, TotalValueToAddress},
        },
    },
};
use error::{CreateError, OpenError};
use store::WalletStore;
use zingolib_file_format::WalletFileError;

pub mod error;
pub mod save;
pub mod store;
pub mod sync;

pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// Wallet struct owned by a [`crate::lightclient::LightClient`], with metadata and immutable wallet data stored outside
/// the read/write lock.
struct WalletMeta {
    store: Arc<dyn WalletStore>,
    /// The chain type, extracted at construction for lock-free access.
    chain_type: ChainType,
    birthday: BlockHeight,
    /// The mnemonic seed phrase, if this is a spending wallet.
    mnemonic: Option<Mnemonic>,
    /// The locked mutable wallet state.
    wallet_data: Arc<RwLock<LightWallet>>,
}

impl std::fmt::Debug for WalletMeta {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WalletMeta")
            .field("chain_type", &self.chain_type)
            .field("birthday", &self.birthday)
            .field("mnemonic", &self.mnemonic)
            .finish()
    }
}

impl WalletMeta {
    /// Creates a new `WalletMeta` by wrapping a [`crate::wallet::LightWallet`] in a lock alongside metadata and
    /// immutable wallet data.
    fn new(store: Arc<dyn WalletStore>, wallet: LightWallet) -> Self {
        Self {
            store,
            chain_type: wallet.chain_type(),
            birthday: wallet.birthday(),
            mnemonic: wallet.mnemonic().cloned(),
            wallet_data: Arc::new(RwLock::new(wallet)),
        }
    }
}

/// How [`LightClient::create`] stores the new wallet.
pub enum Encryption {
    None,
    Passphrase(EncryptionConfig),
}

/// How [`LightClient::open`] reads the stored wallet.
pub enum Unlock {
    Plaintext,
    Passphrase(String),
}

/// Struct which owns and manages the [`crate::wallet::LightWallet`]. Responsible for network operations such as
/// storing the indexer URI, creating gRPC clients and syncing the wallet to the blockchain.
///
/// `sync_mode` publishes the [`pepper_sync::wallet::SyncMode`] to the sync engine, which wakes on every change.
pub struct LightClient {
    pub(crate) indexer: zingo_netutils::GrpcIndexer,
    wallet: WalletMeta,
    sync_mode: watch::Sender<SyncMode>,
    sync_handle: Option<JoinHandle<Result<SyncResult, SyncError<WalletError>>>>,
    sync_events: SyncEmitter,
    save_active: Arc<AtomicBool>,
    save_handle: Option<JoinHandle<std::io::Result<()>>>,
}

impl LightClient {
    /// Creates a wallet and its client. Fails if `store` already holds a wallet.
    #[allow(clippy::result_large_err)]
    pub async fn create(
        store: impl WalletStore,
        chain_type: ChainType,
        indexer_uri: http::Uri,
        wallet_config: WalletConfig,
        encryption: Encryption,
    ) -> Result<Self, CreateError> {
        if store.load()?.is_some() {
            return Err(CreateError::AlreadyExists);
        }
        let encryption = match encryption {
            Encryption::None => None,
            Encryption::Passphrase(config) => Some(config),
        };
        let wallet = LightWallet::new(chain_type, wallet_config, encryption)?;

        Ok(Self::assemble(store, indexer_uri, wallet).await?)
    }

    /// Opens the wallet held by `store`. `unlock` must match how the wallet was stored.
    #[allow(clippy::result_large_err)]
    pub async fn open(
        store: impl WalletStore,
        chain_type: ChainType,
        indexer_uri: http::Uri,
        unlock: Unlock,
    ) -> Result<Self, OpenError> {
        let bytes = store.load()?.ok_or(OpenError::NotFound)?;
        let passphrase = match (unlock, encryption::is_encrypted(&bytes)) {
            (Unlock::Plaintext, false) => None,
            (Unlock::Passphrase(passphrase), true) => Some(passphrase),
            (Unlock::Plaintext, true) => return Err(OpenError::PassphraseRequired),
            (Unlock::Passphrase(_), false) => return Err(OpenError::NotEncrypted),
        };
        let wallet = LightWallet::read_encrypted(bytes.as_slice(), chain_type, passphrase)
            .map_err(|e| match e {
                WalletError::File(WalletFileError::Encryption(
                    WalletEncryptionError::DecryptionFailed,
                )) => OpenError::DecryptionFailed,
                e => OpenError::Wallet(e),
            })?;

        Ok(Self::assemble(store, indexer_uri, wallet).await?)
    }

    async fn assemble(
        store: impl WalletStore,
        indexer_uri: http::Uri,
        wallet: LightWallet,
    ) -> Result<Self, zingo_netutils::GetClientError> {
        // Both `ring` and `aws-lc-rs` are enabled through transitive dependencies, so rustls
        // cannot select a provider itself.
        let _ = rustls::crypto::ring::default_provider().install_default();

        let indexer = zingo_netutils::GrpcIndexer::new(indexer_uri).await?;

        let (sync_events, _root_receiver) =
            SyncEmitter::new(wallet.wallet_settings.sync_config.event_channel_capacity);

        Ok(LightClient {
            indexer,
            wallet: WalletMeta::new(Arc::new(store), wallet),
            sync_mode: watch::Sender::new(SyncMode::NotRunning),
            sync_handle: None,
            sync_events,
            save_active: Arc::new(AtomicBool::new(false)),
            save_handle: None,
        })
    }

    /// Returns the chain type for lock-free access.
    pub fn chain_type(&self) -> ChainType {
        self.wallet.chain_type
    }

    /// Returns the wallet birthday height for lock-free access.
    pub fn birthday(&self) -> u32 {
        u32::from(self.wallet.birthday)
    }

    pub fn mnemonic_phrase(&self) -> Option<String> {
        self.wallet
            .mnemonic
            .as_ref()
            .map(|m| m.phrase().to_string())
    }

    /// Returns a reference to the locked mutable wallet state.
    // TODO: remove this from public API and replace with APIs to pass through all wallet methods without the consumer having access to the rwlock
    pub fn wallet(&self) -> &Arc<RwLock<LightWallet>> {
        &self.wallet.wallet_data
    }

    /// Returns URI of the indexer the lightclient is connected to.
    pub fn indexer_uri(&self) -> &http::Uri {
        self.indexer.uri()
    }

    /// Set indexer URI.
    ///
    /// Replaces the current gRPC client(s) with new ones that point at the provided URI.
    pub async fn set_indexer_uri(
        &mut self,
        server: http::Uri,
    ) -> Result<(), zingo_netutils::GetClientError> {
        self.indexer = zingo_netutils::GrpcIndexer::new(server).await?;
        Ok(())
    }

    /// Returns server information.
    pub async fn server_info(&mut self) -> Result<LightdInfo, zingo_netutils::Status> {
        self.indexer.get_lightd_info(DEFAULT_REQUEST_TIMEOUT).await
    }

    /// Wrapper for [`crate::wallet::LightWallet::generate_unified_address`].
    pub async fn generate_unified_address(
        &mut self,
        receivers: ReceiverSelection,
        account_id: zip32::AccountId,
    ) -> Result<(UnifiedAddressId, UnifiedAddress), KeyError> {
        self.wallet()
            .write()
            .await
            .generate_unified_address(receivers, account_id)
    }

    /// Wrapper for [`crate::wallet::LightWallet::generate_transparent_address`].
    pub async fn generate_transparent_address(
        &mut self,
        account_id: zip32::AccountId,
        enforce_no_gap: bool,
    ) -> Result<(TransparentAddressId, TransparentAddress), KeyError> {
        self.wallet()
            .write()
            .await
            .generate_transparent_address(account_id, enforce_no_gap)
    }

    /// Returns the wallet's unified addresses and their derivation identifiers.
    pub async fn unified_addresses(&self) -> BTreeMap<UnifiedAddressId, UnifiedAddress> {
        self.wallet().read().await.unified_addresses().clone()
    }

    /// Returns the wallet's transparent addresses and their derivation identifiers.
    pub async fn transparent_addresses(&self) -> BTreeMap<TransparentAddressId, String> {
        self.wallet().read().await.transparent_addresses().clone()
    }

    /// Wrapper for [`crate::wallet::LightWallet::account_balance`].
    pub async fn account_balance(
        &self,
        account_id: zip32::AccountId,
    ) -> Result<AccountBalance, BalanceError> {
        self.wallet().read().await.account_balance(account_id)
    }

    /// Wrapper for [`crate::wallet::LightWallet::transaction_summaries`].
    pub async fn transaction_summaries(
        &self,
        reverse_sort: bool,
    ) -> Result<TransactionSummaries, SummaryError> {
        self.wallet()
            .read()
            .await
            .transaction_summaries(reverse_sort)
            .await
    }

    /// Wrapper for [`crate::wallet::LightWallet::transaction_summary`].
    pub async fn transaction_summary(
        &self,
        txid: TxId,
    ) -> Result<Option<TransactionSummary>, SummaryError> {
        self.wallet().read().await.transaction_summary(txid)
    }

    /// Wrapper for [`crate::wallet::LightWallet::wallet_events`].
    pub async fn wallet_events(
        &self,
        sort_highest_to_lowest: bool,
    ) -> Result<WalletEvents, SummaryError> {
        self.wallet()
            .read()
            .await
            .wallet_events(sort_highest_to_lowest)
            .await
    }

    /// Wrapper for [`crate::wallet::LightWallet::messages_containing`].
    pub async fn messages_containing(
        &self,
        filter: Option<&str>,
    ) -> Result<WalletEvents, SummaryError> {
        self.wallet().read().await.messages_containing(filter).await
    }

    /// Wrapper for [`crate::wallet::LightWallet::do_total_memobytes_to_address`].
    pub async fn do_total_memobytes_to_address(
        &self,
    ) -> Result<TotalMemoBytesToAddress, SummaryError> {
        self.wallet()
            .read()
            .await
            .do_total_memobytes_to_address()
            .await
    }

    /// Wrapper for [`crate::wallet::LightWallet::do_total_spends_to_address`].
    pub async fn do_total_spends_to_address(&self) -> Result<TotalSendsToAddress, SummaryError> {
        self.wallet()
            .read()
            .await
            .do_total_spends_to_address()
            .await
    }

    /// Wrapper for [`crate::wallet::LightWallet::do_total_value_to_address`].
    pub async fn do_total_value_to_address(&self) -> Result<TotalValueToAddress, SummaryError> {
        self.wallet().read().await.do_total_value_to_address().await
    }
}

impl std::fmt::Debug for LightClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LightClient")
            .field("indexer", &self.indexer)
            .field("wallet_meta", &self.wallet)
            .field("sync_mode", &self.sync_mode())
            .field(
                "save_active",
                &self.save_active.load(std::sync::atomic::Ordering::Acquire),
            )
            .finish()
    }
}

#[cfg(test)]
mod darkside;
#[cfg(test)]
mod mock_chain_tests;

#[cfg(test)]
mod tests {
    use crate::{
        config::{ChainType, DEFAULT_INDEXER_URI, WalletConfig},
        lightclient::{
            Encryption, LightClient, Unlock,
            error::{CreateError, OpenError},
            store::FileStore,
        },
        testutils::default_test_wallet_settings,
    };
    use tempfile::TempDir;
    use zingo_common_components::protocol::ActivationHeights;
    use zingo_test_vectors::seeds::CHIMNEY_BETTER_SEED;

    #[tokio::test]
    async fn create_then_open() {
        let temp_dir = TempDir::new().unwrap();
        let store = FileStore::new(temp_dir.path().join("wallet.dat"));
        let chain_type = ChainType::Regtest(ActivationHeights::default());
        let indexer_uri: http::Uri = DEFAULT_INDEXER_URI.parse().unwrap();
        let open =
            |unlock| LightClient::open(store.clone(), chain_type, indexer_uri.clone(), unlock);
        let create = || {
            LightClient::create(
                store.clone(),
                chain_type,
                indexer_uri.clone(),
                WalletConfig::MnemonicPhrase {
                    mnemonic_phrase: CHIMNEY_BETTER_SEED.to_string(),
                    no_of_accounts: 1.try_into().unwrap(),
                    birthday: 1,
                    wallet_settings: default_test_wallet_settings(),
                },
                Encryption::None,
            )
        };

        assert!(matches!(
            open(Unlock::Plaintext).await,
            Err(OpenError::NotFound)
        ));

        let mut lc = create().await.unwrap();
        lc.save_task().await;
        lc.wait_for_save().await;

        assert!(matches!(create().await, Err(CreateError::AlreadyExists)));
        assert!(matches!(
            open(Unlock::Passphrase("unused".to_string())).await,
            Err(OpenError::NotEncrypted)
        ));
        let reopened = open(Unlock::Plaintext).await.unwrap();
        assert_eq!(reopened.mnemonic_phrase(), lc.mnemonic_phrase());

        // The first transparent address and unified address should be derived
        let transparent_addresses = lc.transparent_addresses().await;
        assert_eq!(
            "tmYd5GP6JxUxTUcz98NLPumEotvaMPaXytz",
            transparent_addresses.values().next().unwrap()
        );
        let unified_addresses = lc.unified_addresses().await;
        assert_eq!(
            "uregtest15en5x5cnsc7ye3wfy0prnh3ut34ns9w40htunlh9htfl6k5p004ja5gprxfz8fygjeax07a8489wzjk8gsx65thcp6d3ku8umgaka6f0",
            unified_addresses
                .values()
                .next()
                .unwrap()
                .encode(&lc.chain_type())
        );
    }
}
