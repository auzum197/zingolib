//! The wallet task. Owns the `LightClient`, runs the sync lifecycle, and translates wallet
//! activity into [`Action`]s for the reducer. The UI never touches the client directly.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use pepper_sync::{
    events::{SequencedSyncEvent, SyncEvent},
    keys::transparent::TransparentScope,
    sync::ScanPriority,
};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use zcash_protocol::{PoolType, ShieldedPool};
use zingo_common_components::protocol::ActivationHeights;
use zingolib::config::{ChainType, ClientConfig, WalletConfig, construct_lightwalletd_uri};
use zingolib::data::PollReport;
use zingolib::lightclient::LightClient;
use zingolib::wallet::WalletSettings;
use zingolib::wallet::encryption::{Argon2Params, EncryptionConfig};
use zingolib::wallet::keys::unified::ReceiverSelection;
use zingolib::wallet::output::SpendStatus;
use zingolib::wallet::summary::data::{
    SelfSendWalletEvent, SendType, SentWalletEvent, TransactionKind, TransactionSummary,
    WalletEvent, WalletEventKind,
};

use crate::app::{
    Action, AddressKind, AddressRow, Balance, ChainRange, Command, InputRow, Network, OpenSpec,
    OutputRole, OutputRow, Pool, PoolBalance, RangePriority, SpendState, SyncMode, SyncNote,
    TxKind, TxRow, TxStatus, WalletData, WalletEventRow, WalletInfo,
};

/// Pause between the end of one sync session and the start of the next.
const RESYNC_INTERVAL: Duration = Duration::from_secs(20);
/// Pause before retrying after a failed session.
const RETRY_INTERVAL: Duration = Duration::from_secs(30);
const TICK: Duration = Duration::from_millis(500);
/// Longest we wait for a running session to stop on quit.
const STOP_TIMEOUT: Duration = Duration::from_secs(120);
const SAVE_TIMEOUT: Duration = Duration::from_secs(30);

/// Fixed facts the backend needs before a wallet exists.
#[derive(Clone, Debug)]
pub struct BackendConfig {
    pub network: Network,
    pub data_dir: Option<PathBuf>,
    pub wallet_name: String,
    pub server: Option<String>,
    pub kdf_memory_mib: u32,
}

impl BackendConfig {
    /// The wallet directory for `network`: the flag if given, else zingolib's default.
    pub fn wallet_dir(&self, network: Network) -> PathBuf {
        match &self.data_dir {
            Some(dir) => dir.clone(),
            None => ClientConfig::builder()
                .set_chain_type(chain_type(network))
                .build()
                .wallet_dir(),
        }
    }

    pub fn wallet_path(&self, network: Network) -> PathBuf {
        self.wallet_dir(network).join(&self.wallet_name)
    }

    fn server_uri(
        &self,
        network: Network,
        override_server: Option<&str>,
    ) -> Result<http::Uri, String> {
        let raw = override_server
            .map(str::to_string)
            .or_else(|| self.server.clone())
            .unwrap_or_else(|| network.default_server().to_string());
        construct_lightwalletd_uri(Some(raw.clone()))
            .map_err(|e| format!("invalid server URL {raw}: {e}"))
    }
}

pub fn chain_type(network: Network) -> ChainType {
    match network {
        Network::Mainnet => ChainType::Mainnet,
        Network::Testnet => ChainType::Testnet,
        Network::Regtest => ChainType::Regtest(ActivationHeights::default()),
    }
}

fn wallet_settings() -> WalletSettings {
    use pepper_sync::config::{PerformanceLevel, SyncConfig, TransparentAddressDiscovery};
    WalletSettings {
        sync_config: SyncConfig {
            transparent_address_discovery: TransparentAddressDiscovery::minimal(),
            performance_level: PerformanceLevel::High,
            ..SyncConfig::default()
        },
        ..WalletSettings::default()
    }
}

/// Spawns the backend task. Commands arrive on `commands`, actions leave on `actions`.
pub fn spawn(
    config: BackendConfig,
    commands: mpsc::UnboundedReceiver<Command>,
    actions: mpsc::UnboundedSender<Action>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(Backend::new(config, actions).run(commands))
}

struct Backend {
    config: BackendConfig,
    actions: mpsc::UnboundedSender<Action>,
    lc: Option<LightClient>,
    network: Network,
    auto: bool,
    next_launch: Option<Instant>,
    dirty: bool,
    last_mode: SyncMode,
}

impl Backend {
    fn new(config: BackendConfig, actions: mpsc::UnboundedSender<Action>) -> Self {
        Self {
            network: config.network,
            config,
            actions,
            lc: None,
            auto: true,
            next_launch: None,
            dirty: false,
            last_mode: SyncMode::NotRunning,
        }
    }

    fn send(&self, action: Action) {
        let _ = self.actions.send(action);
    }

    fn sync_note(&self, note: SyncNote) {
        self.send(Action::Sync(note, Instant::now()));
    }

    async fn run(mut self, mut commands: mpsc::UnboundedReceiver<Command>) {
        let mut events: Option<tokio::sync::broadcast::Receiver<SequencedSyncEvent>> = None;
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                cmd = commands.recv() => match cmd {
                    None => break,
                    Some(cmd) => {
                        if self.handle(cmd, &mut events).await {
                            break;
                        }
                    }
                },
                event = recv_event(&mut events) => match event {
                    Ok(event) => self.on_event(event.event).await,
                    Err(RecvError::Lagged(_)) => self.reconcile().await,
                    Err(RecvError::Closed) => events = None,
                },
                _ = tick.tick() => self.on_tick().await,
            }
        }
    }

    /// Returns `true` when the task should exit.
    async fn handle(
        &mut self,
        cmd: Command,
        events: &mut Option<tokio::sync::broadcast::Receiver<SequencedSyncEvent>>,
    ) -> bool {
        match cmd {
            Command::Open(spec) => {
                if self.lc.is_some() {
                    return false;
                }
                match self.open(spec).await {
                    Ok((lc, info)) => {
                        *events = Some(lc.subscribe_sync_events());
                        self.lc = Some(lc);
                        self.send(Action::WalletOpened(info));
                        if let Some(lc) = self.lc.as_mut() {
                            lc.save_task().await;
                        }
                        self.launch().await;
                        self.refresh().await;
                    }
                    Err(message) => self.send(Action::OpenFailed(message)),
                }
            }
            Command::Refresh => self.refresh().await,
            Command::NewUnifiedAddress { orchard, sapling } => {
                if let Some(lc) = self.lc.as_mut() {
                    match lc
                        .generate_unified_address(
                            ReceiverSelection { orchard, sapling },
                            zip32::AccountId::ZERO,
                        )
                        .await
                    {
                        Ok((id, _)) => {
                            self.send(Action::Notice(format!(
                                "new unified address #{}",
                                id.address_index
                            )));
                            self.refresh().await;
                        }
                        Err(e) => self.send(Action::Notice(format!("cannot derive address: {e}"))),
                    }
                }
            }
            Command::NewTransparentAddress => {
                if let Some(lc) = self.lc.as_mut() {
                    match lc
                        .generate_transparent_address(zip32::AccountId::ZERO, false)
                        .await
                    {
                        Ok((id, _)) => {
                            self.send(Action::Notice(format!(
                                "new transparent address #{}",
                                id.address_index().index()
                            )));
                            self.refresh().await;
                        }
                        Err(e) => self.send(Action::Notice(format!("cannot derive address: {e}"))),
                    }
                }
            }
            Command::PauseSync => {
                if let Some(lc) = self.lc.as_ref() {
                    match lc.sync_mode() {
                        pepper_sync::wallet::SyncMode::Running => {
                            if let Err(e) = lc.pause_sync() {
                                self.send(Action::Notice(format!("cannot pause: {e}")));
                            }
                        }
                        pepper_sync::wallet::SyncMode::NotRunning => {
                            self.auto = false;
                            self.next_launch = None;
                            self.sync_note(SyncNote::Auto(false));
                        }
                        _ => {}
                    }
                }
            }
            Command::ResumeSync => {
                if let Some(lc) = self.lc.as_ref() {
                    match lc.sync_mode() {
                        pepper_sync::wallet::SyncMode::Paused => {
                            if let Err(e) = lc.resume_sync() {
                                self.send(Action::Notice(format!("cannot resume: {e}")));
                            }
                        }
                        pepper_sync::wallet::SyncMode::NotRunning => {
                            self.auto = true;
                            self.sync_note(SyncNote::Auto(true));
                            self.launch().await;
                        }
                        _ => {}
                    }
                }
            }
            Command::Rescan => {
                if self.lc.is_none() {
                    return false;
                }
                self.sync_note(SyncNote::Reset);
                self.auto = true;
                self.next_launch = None;
                self.sync_note(SyncNote::Auto(true));
                let rescanned = self.lc.as_mut().expect("checked above").rescan().await;
                match rescanned {
                    Ok(()) => self.dirty = true,
                    Err(e) => {
                        self.sync_note(SyncNote::Failed(e.to_string()));
                        let at = Instant::now() + RETRY_INTERVAL;
                        self.next_launch = Some(at);
                        self.sync_note(SyncNote::Scheduled { at });
                    }
                }
                self.refresh().await;
            }
            Command::Quit => {
                self.shutdown().await;
                self.send(Action::Shutdown);
                return true;
            }
            Command::Copy(_) | Command::SaveTheme(_) | Command::ForceQuit => {}
        }
        false
    }

    async fn open(&mut self, spec: OpenSpec) -> Result<(LightClient, WalletInfo), String> {
        let (config, encryption, network) = match spec {
            OpenSpec::Read { passphrase } => {
                let network = self.config.network;
                let config = ClientConfig::builder()
                    .set_chain_type(chain_type(network))
                    .set_indexer_uri(self.config.server_uri(network, None)?)
                    .set_wallet_dir(self.config.wallet_dir(network))
                    .set_wallet_name(self.config.wallet_name.clone())
                    .set_wallet_config(WalletConfig::Read)
                    .build();
                (config, passphrase.map(EncryptionConfig::new), network)
            }
            OpenSpec::Create {
                network,
                server,
                ufvk,
                birthday,
                passphrase,
            } => {
                let path = self.config.wallet_path(network);
                if path.exists() {
                    return Err(format!(
                        "a wallet file already exists at {}",
                        path.display()
                    ));
                }
                let config = ClientConfig::builder()
                    .set_chain_type(chain_type(network))
                    .set_indexer_uri(self.config.server_uri(network, Some(&server))?)
                    .set_wallet_dir(self.config.wallet_dir(network))
                    .set_wallet_name(self.config.wallet_name.clone())
                    .set_wallet_config(WalletConfig::Ufvk {
                        ufvk,
                        birthday,
                        wallet_settings: wallet_settings(),
                    })
                    .build();
                let encryption = passphrase.map(|p| {
                    EncryptionConfig::with_params(
                        p,
                        Argon2Params::with_memory_mib(self.config.kdf_memory_mib),
                    )
                });
                (config, encryption, network)
            }
        };
        let wallet_path = config.get_wallet_path().to_path_buf();
        let lc = LightClient::new(config, false, encryption)
            .await
            .map_err(|e| friendly_open_error(&e.to_string()))?;
        let has_orchard = {
            let wallet = lc.wallet().read().await;
            if wallet
                .unified_key_store
                .values()
                .any(|k| k.is_spending_key())
            {
                return Err(
                    "this wallet file holds spending keys; zingolib-tui only opens watch-only wallets"
                        .into(),
                );
            }
            let Some(keys) = wallet.unified_key_store.get(&zip32::AccountId::ZERO) else {
                return Err("the wallet file has no viewing key".into());
            };
            keys.default_receivers().is_some_and(|r| r.orchard)
        };
        self.network = network;
        let info = WalletInfo {
            network,
            wallet_name: self.config.wallet_name.clone(),
            wallet_path: wallet_path.display().to_string(),
            birthday: lc.birthday(),
            server: lc.indexer_uri().to_string(),
            has_orchard,
        };
        Ok((lc, info))
    }

    async fn launch(&mut self) {
        if self.lc.is_none() {
            return;
        }
        self.next_launch = None;
        let launched = self.lc.as_mut().expect("checked above").sync().await;
        match launched {
            Ok(()) => {}
            Err(e) => {
                self.sync_note(SyncNote::Failed(e.to_string()));
                if self.auto {
                    let at = Instant::now() + RETRY_INTERVAL;
                    self.next_launch = Some(at);
                    self.sync_note(SyncNote::Scheduled { at });
                }
            }
        }
    }

    async fn on_tick(&mut self) {
        let Some(poll) = self.lc.as_mut().map(LightClient::poll_sync) else {
            return;
        };
        match poll {
            PollReport::Ready(Ok(result)) => {
                self.sync_note(SyncNote::Finished {
                    blocks_scanned: u64::from(result.blocks_scanned),
                });
                self.dirty = true;
                if self.auto {
                    let at = Instant::now() + RESYNC_INTERVAL;
                    self.next_launch = Some(at);
                    self.sync_note(SyncNote::Scheduled { at });
                }
            }
            PollReport::Ready(Err(e)) => {
                self.sync_note(SyncNote::Failed(e.to_string()));
                self.dirty = true;
                if self.auto {
                    let at = Instant::now() + RETRY_INTERVAL;
                    self.next_launch = Some(at);
                    self.sync_note(SyncNote::Scheduled { at });
                }
            }
            PollReport::NotReady => {}
            PollReport::NoHandle => {
                if self.auto && self.next_launch.is_some_and(|at| at <= Instant::now()) {
                    self.launch().await;
                }
            }
        }
        if let Some(mode) = self.lc.as_ref().map(|lc| match lc.sync_mode() {
            pepper_sync::wallet::SyncMode::NotRunning => SyncMode::NotRunning,
            pepper_sync::wallet::SyncMode::Running => SyncMode::Running,
            pepper_sync::wallet::SyncMode::Paused => SyncMode::Paused,
            pepper_sync::wallet::SyncMode::Shutdown => SyncMode::Stopping,
        }) && mode != self.last_mode
        {
            self.last_mode = mode;
            self.sync_note(SyncNote::Mode(mode));
        }
        let save = match self.lc.as_mut() {
            Some(lc) => lc.check_save_error().await,
            None => Ok(()),
        };
        if let Err(e) = save {
            self.send(Action::Notice(format!("wallet save failed: {e}")));
        }
        if self.dirty {
            self.dirty = false;
            self.refresh().await;
        }
    }

    async fn on_event(&mut self, event: SyncEvent) {
        match event {
            SyncEvent::SessionStarted {
                tip,
                birthday,
                total_sapling_outputs,
                total_orchard_outputs,
                total_ironwood_outputs,
                already_scanned_sapling_outputs,
                already_scanned_orchard_outputs,
                already_scanned_ironwood_outputs,
                already_scanned_blocks,
                ..
            } => self.sync_note(SyncNote::SessionStarted {
                tip: u32::from(tip),
                birthday: u32::from(birthday),
                total_outputs: u64::from(total_sapling_outputs)
                    + u64::from(total_orchard_outputs)
                    + u64::from(total_ironwood_outputs),
                scanned_outputs: u64::from(already_scanned_sapling_outputs)
                    + u64::from(already_scanned_orchard_outputs)
                    + u64::from(already_scanned_ironwood_outputs),
                scanned_blocks: u64::from(already_scanned_blocks),
            }),
            SyncEvent::ScanPlanUpdated { ranges } => {
                self.sync_note(SyncNote::ScanPlanUpdated {
                    ranges: ranges
                        .into_iter()
                        .map(|range| ChainRange {
                            start: u32::from(range.block_range().start),
                            end: u32::from(range.block_range().end),
                            priority: range_priority(range.priority()),
                        })
                        .collect(),
                });
            }
            SyncEvent::BatchScanStarted {
                range,
                sapling_outputs,
                orchard_outputs,
                ironwood_outputs,
                ..
            } => self.sync_note(SyncNote::BatchScanStarted {
                start: u32::from(range.start),
                end: u32::from(range.end),
                outputs: u64::from(sapling_outputs)
                    + u64::from(orchard_outputs)
                    + u64::from(ironwood_outputs),
            }),
            SyncEvent::BatchScanCompleted { range } => {
                self.sync_note(SyncNote::BatchScanCompleted {
                    start: u32::from(range.start),
                    end: u32::from(range.end),
                });
            }
            SyncEvent::BatchCommitStarted { range } => {
                self.sync_note(SyncNote::BatchCommitStarted {
                    start: u32::from(range.start),
                    end: u32::from(range.end),
                });
            }
            SyncEvent::RangeScanned {
                range,
                sapling_outputs,
                orchard_outputs,
                ironwood_outputs,
                timing,
                ..
            } => self.sync_note(SyncNote::RangeScanned {
                start: u32::from(range.start),
                end: u32::from(range.end),
                outputs: u64::from(sapling_outputs)
                    + u64::from(orchard_outputs)
                    + u64::from(ironwood_outputs),
                duration: timing.total(),
            }),
            SyncEvent::TxDiscovered { txid, status } => {
                self.dirty = true;
                self.sync_note(SyncNote::TxDiscovered {
                    txid: txid.to_string(),
                    confirmed: status.is_confirmed(),
                });
            }
            SyncEvent::Reorg { reverted_to } => {
                self.dirty = true;
                self.sync_note(SyncNote::Reorg {
                    reverted_to: u32::from(reverted_to),
                });
            }
            SyncEvent::TipMoved { to } => {
                self.dirty = true;
                self.sync_note(SyncNote::TipMoved { to: u32::from(to) });
            }
        }
    }

    async fn reconcile(&mut self) {
        let Some(lc) = self.lc.as_ref() else { return };
        let wallet = lc.wallet().read().await;
        if let Ok(status) = pepper_sync::sync_status(&*wallet).await {
            self.sync_note(SyncNote::Reconciled {
                scanned_outputs: u64::from(status.total_sapling_outputs_scanned)
                    + u64::from(status.total_orchard_outputs_scanned)
                    + u64::from(status.total_ironwood_outputs_scanned),
                scanned_blocks: u64::from(status.total_blocks_scanned),
            });
        }
        drop(wallet);
        self.dirty = true;
    }

    async fn refresh(&mut self) {
        let Some(lc) = self.lc.as_ref() else { return };
        match load(lc).await {
            Ok(data) => self.send(Action::DataLoaded(data)),
            Err(e) => self.send(Action::Notice(format!("cannot read wallet: {e}"))),
        }
    }

    async fn shutdown(&mut self) {
        self.auto = false;
        let Some(lc) = self.lc.as_mut() else { return };
        if lc.sync_mode() != pepper_sync::wallet::SyncMode::NotRunning {
            let _ = lc.stop_sync();
            let deadline = Instant::now() + STOP_TIMEOUT;
            while matches!(lc.poll_sync(), PollReport::NotReady) && Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
        let _ = tokio::time::timeout(SAVE_TIMEOUT, lc.wait_for_save()).await;
        if let Err(e) = lc.shutdown_save_task().await {
            tracing::error!("final wallet save failed: {e}");
        }
    }
}

fn range_priority(priority: ScanPriority) -> RangePriority {
    match priority {
        ScanPriority::RefetchingNullifiers => RangePriority::RefetchingNullifiers,
        ScanPriority::Scanning => RangePriority::Scanning,
        ScanPriority::Scanned => RangePriority::Scanned,
        ScanPriority::ScannedWithoutMapping => RangePriority::ScannedWithoutMapping,
        ScanPriority::Historic => RangePriority::Historic,
        ScanPriority::OpenAdjacent => RangePriority::OpenAdjacent,
        ScanPriority::FoundNote => RangePriority::FoundNote,
        ScanPriority::ChainTip => RangePriority::ChainTip,
        ScanPriority::Verify => RangePriority::Verify,
    }
}

async fn recv_event(
    events: &mut Option<tokio::sync::broadcast::Receiver<SequencedSyncEvent>>,
) -> Result<SequencedSyncEvent, RecvError> {
    match events {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

fn friendly_open_error(raw: &str) -> String {
    if raw.contains("decryption failed") {
        "wrong passphrase or corrupt wallet file".into()
    } else {
        raw.to_string()
    }
}

/// Reads everything the screens show.
async fn load(lc: &LightClient) -> Result<WalletData, String> {
    let account = zip32::AccountId::ZERO;
    let balance = lc
        .account_balance(account)
        .await
        .map_err(|e| e.to_string())?;
    fn zats<Z: Into<u64>>(z: Option<Z>) -> u64 {
        z.map_or(0, Into::into)
    }
    let balance = Balance {
        orchard: PoolBalance {
            confirmed: zats(balance.confirmed_orchard_balance),
            pending: zats(balance.unconfirmed_orchard_balance),
        },
        ironwood: PoolBalance {
            confirmed: zats(balance.confirmed_ironwood_balance),
            pending: zats(balance.unconfirmed_ironwood_balance),
        },
        sapling: PoolBalance {
            confirmed: zats(balance.confirmed_sapling_balance),
            pending: zats(balance.unconfirmed_sapling_balance),
        },
        transparent: PoolBalance {
            confirmed: zats(balance.confirmed_transparent_balance),
            pending: zats(balance.unconfirmed_transparent_balance),
        },
    };

    let summaries = lc
        .transaction_summaries(false)
        .await
        .map_err(|e| e.to_string())?;
    let events = lc.wallet_events(false).await.map_err(|e| e.to_string())?;
    let mut by_txid: std::collections::HashMap<String, Vec<WalletEventRow>> =
        std::collections::HashMap::new();
    for event in events.iter() {
        by_txid
            .entry(event.txid.to_string())
            .or_default()
            .push(wallet_event_row(event));
    }
    let mut index = output_index(lc).await;
    let mut txs: Vec<TxRow> = summaries
        .iter()
        .map(|s| tx_row(s, &mut by_txid, &mut index))
        .collect();
    txs.sort_by(|a, b| b.height.cmp(&a.height).then(b.datetime.cmp(&a.datetime)));

    let received = lc
        .do_total_value_to_address()
        .await
        .map(|t| t.0)
        .unwrap_or_default();
    let mut addresses = Vec::new();
    for (id, unified_address) in lc.unified_addresses().await {
        let address = unified_address.encode(&lc.chain_type());
        addresses.push(AddressRow {
            kind: AddressKind::Unified {
                orchard: unified_address.has_orchard(),
                sapling: unified_address.has_sapling(),
                transparent: unified_address.has_transparent(),
            },
            index: id.address_index,
            received: received.get(&address).copied().unwrap_or(0),
            address,
        });
    }
    // change and refund addresses are the wallet's own plumbing; only receiving addresses
    // mean anything to the user
    for (id, address) in lc
        .transparent_addresses()
        .await
        .into_iter()
        .filter(|(id, _)| id.scope() == TransparentScope::External)
    {
        addresses.push(AddressRow {
            kind: AddressKind::Transparent {
                scope: id.scope().to_string(),
            },
            index: id.address_index().index(),
            received: received.get(&address).copied().unwrap_or(0),
            address,
        });
    }

    Ok(WalletData {
        balance,
        txs,
        addresses,
    })
}

/// Wallet output facts the per-transaction summaries do not carry: which transaction consumed
/// each output, and whether an output was received externally or is change.
#[derive(Default)]
struct OutputIndex {
    /// Inputs keyed by the transaction that spent them.
    spent_by: std::collections::HashMap<String, Vec<InputRow>>,
    /// Scope keyed by the transaction that created the output, its pool, and its index.
    scopes: std::collections::HashMap<(String, Pool, u32), String>,
}

/// Walks every wallet output once, in every pool, to build [`OutputIndex`].
async fn output_index(lc: &LightClient) -> OutputIndex {
    use pepper_sync::wallet::{IronwoodNote, OrchardNote, SaplingNote};

    let wallet = lc.wallet().read().await;
    let mut index = OutputIndex::default();

    let pools = [
        (Pool::Orchard, wallet.note_summaries::<OrchardNote>(true)),
        (Pool::Ironwood, wallet.note_summaries::<IronwoodNote>(true)),
        (Pool::Sapling, wallet.note_summaries::<SaplingNote>(true)),
    ];
    for (pool, summaries) in pools {
        for note in summaries.iter() {
            let source_txid = note.txid.to_string();
            index.scopes.insert(
                (source_txid.clone(), pool, note.output_index),
                note.scope.to_string(),
            );
            if let Some((spender, pending)) = spend_target(&note.spend_status) {
                index.spent_by.entry(spender).or_default().push(InputRow {
                    pool,
                    value: note.value,
                    source_txid,
                    output_index: note.output_index,
                    memo: note.memo.clone(),
                    pending,
                });
            }
        }
    }
    for coin in wallet.coin_summaries(true) {
        let source_txid = coin.txid.to_string();
        index.scopes.insert(
            (source_txid.clone(), Pool::Transparent, coin.output_index),
            coin.scope.to_string(),
        );
        if let Some((spender, pending)) = spend_target(&coin.spend_status) {
            index.spent_by.entry(spender).or_default().push(InputRow {
                pool: Pool::Transparent,
                value: coin.value,
                source_txid,
                output_index: coin.output_index,
                memo: None,
                pending,
            });
        }
    }
    index
}

/// The transaction consuming an output, and whether that spend is still unconfirmed.
fn spend_target(status: &SpendStatus) -> Option<(String, bool)> {
    match status {
        SpendStatus::Unspent => None,
        SpendStatus::Spent(txid) => Some((txid.to_string(), false)),
        SpendStatus::CalculatedSpent(txid)
        | SpendStatus::TransmittedSpent(txid)
        | SpendStatus::MempoolSpent(txid) => Some((txid.to_string(), true)),
    }
}

fn spend_state(status: &SpendStatus) -> SpendState {
    match spend_target(status) {
        None => SpendState::Unspent,
        Some((txid, true)) => SpendState::PendingSpent(txid),
        Some((txid, false)) => SpendState::Spent(txid),
    }
}

fn tx_row(
    s: &TransactionSummary,
    events: &mut std::collections::HashMap<String, Vec<WalletEventRow>>,
    index: &mut OutputIndex,
) -> TxRow {
    let txid = s.txid.to_string();
    let status = match s.status {
        st if st.is_confirmed() => TxStatus::Confirmed(u32::from(st.get_height())),
        st if st.is_failed() => TxStatus::Failed,
        _ => TxStatus::Pending,
    };
    let kind = match s.kind {
        TransactionKind::Received => TxKind::Received,
        TransactionKind::Sent(SendType::Send) => TxKind::Sent,
        TransactionKind::Sent(SendType::Shield) => TxKind::Shield,
        TransactionKind::Sent(SendType::SendToSelf) => TxKind::SentToSelf,
        TransactionKind::Sent(SendType::PoolMove { to }) => TxKind::Moved(pool_of(to)),
    };

    let scope_of = |pool: Pool, output_index: u32| {
        index
            .scopes
            .get(&(txid.clone(), pool, output_index))
            .cloned()
    };
    let mut received = Vec::new();
    let notes = [
        (Pool::Orchard, &s.orchard_notes),
        (Pool::Ironwood, &s.ironwood_notes),
        (Pool::Sapling, &s.sapling_notes),
    ];
    for (pool, notes) in notes {
        for n in notes {
            received.push(OutputRow {
                pool,
                role: OutputRole::Received,
                value: n.value,
                index: n.output_index,
                address: None,
                memo: n.memo.clone(),
                spend: spend_state(&n.spend_status),
                scope: scope_of(pool, n.output_index),
            });
        }
    }
    for c in &s.transparent_coins {
        received.push(OutputRow {
            pool: Pool::Transparent,
            role: OutputRole::Received,
            value: c.value,
            index: c.output_index,
            address: None,
            memo: None,
            spend: spend_state(&c.spend_summary),
            scope: scope_of(Pool::Transparent, c.output_index),
        });
    }

    let mut outgoing = Vec::new();
    let notes = [
        (Pool::Orchard, &s.outgoing_orchard_notes),
        (Pool::Ironwood, &s.outgoing_ironwood_notes),
        (Pool::Sapling, &s.outgoing_sapling_notes),
    ];
    for (pool, notes) in notes {
        for n in notes {
            outgoing.push(OutputRow {
                pool,
                role: OutputRole::Sent,
                value: n.value,
                index: n.output_index,
                address: Some(
                    n.recipient_unified_address
                        .clone()
                        .unwrap_or_else(|| n.recipient.clone()),
                ),
                memo: n.memo.clone(),
                spend: SpendState::Unspent,
                scope: Some(n.scope.to_string()),
            });
        }
    }
    for c in &s.outgoing_transparent_coins {
        outgoing.push(OutputRow {
            pool: Pool::Transparent,
            role: OutputRole::Sent,
            value: c.value,
            index: c.output_index,
            address: Some(c.recipient.clone()),
            memo: None,
            spend: SpendState::Unspent,
            scope: None,
        });
    }

    let mut inputs = index.spent_by.remove(&txid).unwrap_or_default();
    inputs.sort_by(|a, b| {
        a.source_txid
            .cmp(&b.source_txid)
            .then(a.output_index.cmp(&b.output_index))
    });
    let mut input_pools: Vec<Pool> = inputs.iter().map(|i| i.pool).collect();
    input_pools.dedup();
    let outputs = crate::app::roles::assign_roles(kind, &input_pools, received, outgoing);

    TxRow {
        wallet_events: events.remove(&txid).unwrap_or_default(),
        txid,
        datetime: s.datetime,
        height: u32::from(s.blockheight),
        status,
        kind,
        value: s.value,
        fee: s.fee,
        inputs,
        outputs,
    }
}

fn pool_of(pool: PoolType) -> Pool {
    match pool {
        PoolType::Transparent => Pool::Transparent,
        PoolType::Shielded(ShieldedPool::Sapling) => Pool::Sapling,
        PoolType::Shielded(ShieldedPool::Orchard) => Pool::Orchard,
        PoolType::Shielded(ShieldedPool::Ironwood) => Pool::Ironwood,
    }
}

fn wallet_event_row(event: &WalletEvent) -> WalletEventRow {
    let kind = match &event.kind {
        WalletEventKind::Received => "received",
        WalletEventKind::Sent(SentWalletEvent::Send) => "sent",
        WalletEventKind::Sent(SentWalletEvent::SendToSelf(SelfSendWalletEvent::Basic)) => {
            "sent to self"
        }
        WalletEventKind::Sent(SentWalletEvent::SendToSelf(SelfSendWalletEvent::Shield)) => "shield",
        WalletEventKind::Sent(SentWalletEvent::SendToSelf(SelfSendWalletEvent::MemoToSelf)) => {
            "memo to self"
        }
        WalletEventKind::Sent(SentWalletEvent::SendToSelf(SelfSendWalletEvent::Refund)) => "refund",
        WalletEventKind::Sent(SentWalletEvent::SendToSelf(SelfSendWalletEvent::PoolMove)) => {
            "moved"
        }
    };
    let kind = match (kind, event.pool_received.as_deref()) {
        ("moved", Some(pool)) => format!("moved to {}", pool.to_lowercase()),
        (kind, _) => kind.to_string(),
    };
    WalletEventRow {
        kind,
        value: event.value,
        recipient: event.recipient_address.clone(),
        pool_received: event.pool_received.clone(),
        memos: event.memos.clone(),
    }
}

#[cfg(test)]
mod darkside;
