use std::{collections::BTreeMap, sync::Arc, time::Duration};

use tokio::{
    sync::{RwLock, watch},
    task::JoinHandle,
};

use bip0039::Mnemonic;
use zcash_keys::address::UnifiedAddress;
use zcash_primitives::transaction::TxId;
use zcash_protocol::consensus::{BlockHeight, NetworkUpgrade, Parameters as _};
use zcash_transparent::address::TransparentAddress;

use chain_router::{
    ChainSource, EventReceiver, Failover, Integrity, RequestEvent, Router, SourceError, SourceSet,
    WalletIdentifier, chain::ServerInfo,
};
use lightwallet_grpc_client::{GrpcIndexer, TcpConnector};
use pepper_sync::{
    SyncStrategy, error::SyncError, events::SyncEmitter, keys::transparent::TransparentAddressId,
    sync::SyncResult, wallet::SyncMode,
};

use crate::{
    config::{Birthday, ChainType, WalletConfig},
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
use save::SaveState;
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
    fn new(wallet: LightWallet) -> Self {
        Self {
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
    sources: Arc<SourceSet>,
    strategy: Arc<dyn SyncStrategy<ChainType, LightWallet>>,
    events: Option<Arc<dyn EventReceiver>>,
    wallet: WalletMeta,
    sync_mode: watch::Sender<SyncMode>,
    sync_handle: Option<JoinHandle<Result<SyncResult, SyncError<WalletError>>>>,
    sync_events: SyncEmitter,
    changes: watch::Receiver<u64>,
    save_state: watch::Receiver<SaveState>,
}

/// Configures the sources, the router and the sync strategy of a [`LightClient`].
pub struct LightClientBuilder {
    chain_type: ChainType,
    sources: Vec<Arc<dyn ChainSource>>,
    router: Arc<dyn Router>,
    strategy: Arc<dyn SyncStrategy<ChainType, LightWallet>>,
    events: Option<Arc<dyn EventReceiver>>,
}

fn source_set(
    sources: Vec<Arc<dyn ChainSource>>,
    router: Arc<dyn Router>,
    events: &Option<Arc<dyn EventReceiver>>,
) -> SourceSet {
    let sources = SourceSet::new(sources, router);
    match events.clone() {
        Some(events) => sources.with_events(move |event: &RequestEvent| events.on_event(event)),
        None => sources,
    }
}

impl LightClientBuilder {
    /// Adds a chain source. The order of the calls is the order of preference.
    pub fn source(mut self, source: impl ChainSource + 'static) -> Self {
        self.sources.push(Arc::new(source));
        self
    }

    /// Adds an indexer at `uri` with the default connector. It can receive `identifiers`.
    pub fn indexer(
        self,
        uri: http::Uri,
        integrity: Integrity,
        identifiers: &[WalletIdentifier],
    ) -> Result<Self, lightwallet_grpc_client::GetClientError> {
        let indexer = GrpcIndexer::with_connector(uri, TcpConnector::default())?
            .with_integrity(integrity)
            .allow_identifiers(identifiers);
        Ok(self.source(indexer))
    }

    /// Sends one [`RequestEvent`] to `receiver` for each request to a source.
    pub fn request_events(mut self, receiver: impl EventReceiver + 'static) -> Self {
        self.events = Some(Arc::new(receiver));
        self
    }

    /// Replaces the default [`Failover`] router.
    pub fn router(mut self, router: impl Router + 'static) -> Self {
        self.router = Arc::new(router);
        self
    }

    /// Replaces the default [`pepper_sync::Auto`] strategy.
    pub fn sync(mut self, strategy: impl SyncStrategy<ChainType, LightWallet> + 'static) -> Self {
        self.strategy = Arc::new(strategy);
        self
    }

    /// Creates a wallet and its client. Fails if `store` already holds a wallet.
    ///
    /// A [`Birthday::ChainTip`] is read from the sources, so it needs network access.
    #[allow(clippy::result_large_err)]
    pub async fn create(
        self,
        store: impl WalletStore,
        mut wallet_config: WalletConfig,
        encryption: Encryption,
    ) -> Result<LightClient, CreateError> {
        if store.load()?.is_some() {
            return Err(CreateError::AlreadyExists);
        }
        let chain_type = self.chain_type;
        let sources = source_set(self.sources, self.router, &self.events);
        let birthday = wallet_config.birthday_mut();
        if *birthday == Birthday::ChainTip {
            let chain_tip = u32::from(sources.tip().await?.value.height);
            let sapling_activation = chain_type
                .activation_height(NetworkUpgrade::Sapling)
                .map_or(0, u32::from);
            *birthday = Birthday::Height(chain_tip.saturating_sub(100).max(sapling_activation));
        }
        let encryption = match encryption {
            Encryption::None => None,
            Encryption::Passphrase(config) => Some(config),
        };
        let mut wallet = LightWallet::new(chain_type, wallet_config, encryption)?;
        if let Some(bytes) = wallet.save()? {
            store.write(&bytes)?;
        }

        Ok(LightClient::assemble(
            store,
            sources,
            self.strategy,
            self.events,
            wallet,
        ))
    }

    /// Opens the wallet held by `store`. `unlock` must match how the wallet was stored.
    #[allow(clippy::result_large_err)]
    pub async fn open(
        self,
        store: impl WalletStore,
        unlock: Unlock,
    ) -> Result<LightClient, OpenError> {
        let bytes = store.load()?.ok_or(OpenError::NotFound)?;
        let passphrase = match (unlock, encryption::is_encrypted(&bytes)) {
            (Unlock::Plaintext, false) => None,
            (Unlock::Passphrase(passphrase), true) => Some(passphrase),
            (Unlock::Plaintext, true) => return Err(OpenError::PassphraseRequired),
            (Unlock::Passphrase(_), false) => return Err(OpenError::NotEncrypted),
        };
        let wallet = LightWallet::read_encrypted(bytes.as_slice(), self.chain_type, passphrase)
            .map_err(|e| match e {
                WalletError::File(WalletFileError::Encryption(
                    WalletEncryptionError::DecryptionFailed,
                )) => OpenError::DecryptionFailed,
                e => OpenError::Wallet(e),
            })?;
        let sources = source_set(self.sources, self.router, &self.events);

        Ok(LightClient::assemble(
            store,
            sources,
            self.strategy,
            self.events,
            wallet,
        ))
    }
}

impl LightClient {
    /// Starts a client configuration with no sources.
    pub fn builder(chain_type: ChainType) -> LightClientBuilder {
        // Both `ring` and `aws-lc-rs` are enabled through transitive dependencies, so rustls
        // cannot select a provider itself.
        let _ = rustls::crypto::ring::default_provider().install_default();

        LightClientBuilder {
            chain_type,
            sources: Vec::new(),
            router: Arc::new(Failover::default()),
            strategy: Arc::new(pepper_sync::Auto),
            events: None,
        }
    }

    /// Creates a wallet and its client with one indexer with assumed integrity, which can
    /// receive every wallet identifier.
    /// Fails if `store` already holds a wallet.
    ///
    /// A [`Birthday::ChainTip`] is read from the indexer, so it needs network access.
    #[allow(clippy::result_large_err)]
    pub async fn create(
        store: impl WalletStore,
        chain_type: ChainType,
        indexer_uri: http::Uri,
        wallet_config: WalletConfig,
        encryption: Encryption,
    ) -> Result<Self, CreateError> {
        Self::builder(chain_type)
            .indexer(indexer_uri, Integrity::Assumed, &WalletIdentifier::ALL)?
            .create(store, wallet_config, encryption)
            .await
    }

    /// Opens the wallet held by `store` with one indexer with assumed integrity, which can
    /// receive every wallet identifier.
    /// `unlock` must match how the wallet was stored.
    #[allow(clippy::result_large_err)]
    pub async fn open(
        store: impl WalletStore,
        chain_type: ChainType,
        indexer_uri: http::Uri,
        unlock: Unlock,
    ) -> Result<Self, OpenError> {
        Self::builder(chain_type)
            .indexer(indexer_uri, Integrity::Assumed, &WalletIdentifier::ALL)?
            .open(store, unlock)
            .await
    }

    fn assemble(
        store: impl WalletStore,
        sources: SourceSet,
        strategy: Arc<dyn SyncStrategy<ChainType, LightWallet>>,
        events: Option<Arc<dyn EventReceiver>>,
        wallet: LightWallet,
    ) -> Self {
        let (sync_events, _root_receiver) =
            SyncEmitter::new(wallet.wallet_settings.sync_config.event_channel_capacity);

        let changes = wallet.subscribe_changes();
        let wallet = WalletMeta::new(wallet);
        let save_state = save::spawn(
            Arc::downgrade(&wallet.wallet_data),
            changes.clone(),
            Arc::new(store),
        );

        LightClient {
            sources: Arc::new(sources),
            strategy,
            events,
            wallet,
            sync_mode: watch::Sender::new(SyncMode::NotRunning),
            sync_handle: None,
            sync_events,
            changes,
            save_state,
        }
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

    /// The source set of this client.
    pub fn source_set(&self) -> &Arc<SourceSet> {
        &self.sources
    }

    /// Replaces the sources with one indexer with assumed integrity, which can receive every
    /// wallet identifier.
    pub async fn set_indexer_uri(
        &mut self,
        server: http::Uri,
    ) -> Result<(), lightwallet_grpc_client::GetClientError> {
        let indexer = GrpcIndexer::with_connector(server, TcpConnector::default())?
            .with_integrity(Integrity::Assumed)
            .allow_identifiers(&WalletIdentifier::ALL);
        self.sources = Arc::new(source_set(
            vec![Arc::new(indexer)],
            Arc::new(Failover::default()),
            &self.events,
        ));
        Ok(())
    }

    /// Returns server information.
    pub async fn server_info(&self) -> Result<ServerInfo, SourceError> {
        Ok(self.sources.info().await?.value)
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
            .field("sources", &self.sources.sources())
            .field("wallet_meta", &self.wallet)
            .field("sync_mode", &self.sync_mode())
            .field("save_state", &*self.save_state.borrow())
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
        config::{Birthday, ChainType, DEFAULT_INDEXER_URI, WalletConfig},
        lightclient::{
            Encryption, LightClient, Unlock,
            error::{CreateError, OpenError},
            store::{FileStore, MemoryStore, WalletStore},
        },
        testutils::default_test_wallet_settings,
        wallet::keys::unified::ReceiverSelection,
    };
    use tempfile::TempDir;
    use zingo_common_components::protocol::ActivationHeights;
    use zingo_test_vectors::seeds::CHIMNEY_BETTER_SEED;

    struct WriteOnce(MemoryStore);

    impl WalletStore for WriteOnce {
        fn load(&self) -> std::io::Result<Option<Vec<u8>>> {
            self.0.load()
        }

        fn write(&self, bytes: &[u8]) -> std::io::Result<()> {
            if self.0.load()?.is_some() {
                return Err(std::io::Error::other("store is full"));
            }
            self.0.write(bytes)
        }

        fn delete(&self) -> std::io::Result<()> {
            self.0.delete()
        }
    }

    #[tokio::test]
    async fn flush_reports_a_failed_save() {
        let mut lc = LightClient::create(
            WriteOnce(MemoryStore::default()),
            ChainType::Regtest(ActivationHeights::default()),
            DEFAULT_INDEXER_URI.parse().unwrap(),
            WalletConfig::MnemonicPhrase {
                mnemonic_phrase: CHIMNEY_BETTER_SEED.to_string(),
                no_of_accounts: 1.try_into().unwrap(),
                birthday: Birthday::Height(1),
                wallet_settings: default_test_wallet_settings(),
            },
            Encryption::None,
        )
        .await
        .unwrap();
        lc.flush().await.unwrap();

        lc.generate_unified_address(ReceiverSelection::all_shielded(), zip32::AccountId::ZERO)
            .await
            .unwrap();
        lc.flush().await.unwrap_err();
    }

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
                    birthday: Birthday::Height(1),
                    wallet_settings: default_test_wallet_settings(),
                },
                Encryption::None,
            )
        };

        assert!(matches!(
            open(Unlock::Plaintext).await,
            Err(OpenError::NotFound)
        ));

        let lc = create().await.unwrap();

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
