//! Entrypoint for sync engine

use std::collections::{BTreeMap, HashMap};
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{self, AtomicBool, AtomicU8, AtomicU64};
use std::time::{Duration, SystemTime};

use tokio::sync::{RwLock, mpsc};

use incrementalmerkletree::{Marking, Retention};
use orchard::tree::MerkleHashOrchard;
use shardtree::store::ShardStore;
use zcash_keys::keys::UnifiedFullViewingKey;
use zcash_primitives::transaction::{Transaction, TxId};
use zcash_protocol::ShieldedProtocol;
use zcash_protocol::consensus::{self, BlockHeight};
use zingo_netutils::lightwallet_protocol::RawTransaction;
use zingo_netutils::{Indexer, TransparentIndexer};
use zip32::AccountId;

use zingo_status::confirmation_status::ConfirmationStatus;

use crate::client::{self, FetchRequest};
use crate::config::{PerformanceLevel, SyncConfig};
use crate::error::{
    ContinuityError, MempoolError, ScanError, ServerError, SyncError, SyncModeError,
    SyncStatusError,
};
use crate::events::{SyncEmitter, SyncEvent, emit_tip_if_advanced};
use crate::keys::transparent::TransparentAddressId;
use crate::scan::ScanResults;
use crate::scan::task::{Scanner, ScannerState};
use crate::scan::transactions::scan_transaction;
use crate::sync::state::truncate_scan_ranges;
use crate::wallet::traits::{
    SyncBlocks, SyncNullifiers, SyncOutPoints, SyncShardTrees, SyncTransactions, SyncWallet,
};
use crate::wallet::{
    InitialSyncState, KeyIdInterface, NoteInterface, NullifierMap, OutputId, OutputInterface,
    ScanTarget, SyncMode, SyncState, WalletBlock, WalletTransaction,
};
use crate::witness::LocatedTreeData;

#[cfg(not(feature = "darkside_test"))]
use crate::witness;

#[cfg(not(feature = "darkside_test"))]
pub(crate) mod transparent;

pub(crate) mod spend;
pub(crate) mod state;

pub(crate) const MAX_REORG_ALLOWANCE: u32 = 100;
const VERIFY_BLOCK_RANGE_SIZE: u32 = 10;

/// A snapshot of the current state of sync: scan range coverage and cumulative scanned counts.
///
/// Carries raw counts and coverage only. Derived metrics such as a progress percentage are
/// computed consumer-side from accumulated [`crate::events::SyncEvent::RangeScanned`] output
/// counts against the totals carried by [`crate::events::SyncEvent::SessionStarted`].
#[derive(Debug, Clone)]
#[allow(missing_docs)]
pub struct SyncStatus {
    pub scan_ranges: Vec<ScanRange>,
    pub sync_start_height: BlockHeight,
    pub total_blocks_scanned: u32,
    pub total_sapling_outputs_scanned: u32,
    pub total_orchard_outputs_scanned: u32,
}

impl std::fmt::Display for SyncStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "blocks scanned: {}, outputs scanned: {} (sapling: {}, orchard: {}), scan ranges: {}",
            self.total_blocks_scanned,
            self.total_sapling_outputs_scanned + self.total_orchard_outputs_scanned,
            self.total_sapling_outputs_scanned,
            self.total_orchard_outputs_scanned,
            self.scan_ranges.len(),
        )
    }
}

impl From<SyncStatus> for json::JsonValue {
    fn from(value: SyncStatus) -> Self {
        let scan_ranges: Vec<json::JsonValue> = value
            .scan_ranges
            .iter()
            .map(|range| {
                json::object! {
                    "priority" => format!("{:?}", range.priority()),
                    "start_block" => range.block_range().start.to_string(),
                    "end_block" => (range.block_range().end - 1).to_string(),
                }
            })
            .collect();

        json::object! {
            "scan_ranges" => scan_ranges,
            "sync_start_height" => u32::from(value.sync_start_height),
            "total_blocks_scanned" => value.total_blocks_scanned,
            "total_sapling_outputs_scanned" => value.total_sapling_outputs_scanned,
            "total_orchard_outputs_scanned" => value.total_orchard_outputs_scanned,
        }
    }
}

/// Returned when [`crate::sync::sync`] successfully completes.
#[derive(Debug, Clone)]
#[allow(missing_docs)]
pub struct SyncResult {
    pub sync_start_height: BlockHeight,
    pub sync_end_height: BlockHeight,
    pub blocks_scanned: u32,
    pub sapling_outputs_scanned: u32,
    pub orchard_outputs_scanned: u32,
}

impl std::fmt::Display for SyncResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Sync completed succesfully:
{{
    sync start height: {}
    sync end height: {}
    blocks scanned: {}
    sapling outputs scanned: {}
    orchard outputs scanned: {}
}}",
            self.sync_start_height,
            self.sync_end_height,
            self.blocks_scanned,
            self.sapling_outputs_scanned,
            self.orchard_outputs_scanned,
        )
    }
}

impl From<SyncResult> for json::JsonValue {
    fn from(value: SyncResult) -> Self {
        json::object! {
            "sync_start_height" => u32::from(value.sync_start_height),
            "sync_end_height" => u32::from(value.sync_end_height),
            "blocks_scanned" => value.blocks_scanned,
            "sapling_outputs_scanned" => value.sapling_outputs_scanned,
            "orchard_outputs_scanned" => value.orchard_outputs_scanned,
        }
    }
}

/// Scanning range priority levels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ScanPriority {
    /// Block ranges that are currently refetching nullifiers.
    RefetchingNullifiers,
    /// Block ranges that are currently being scanned.
    Scanning,
    /// Block ranges that have already been scanned will not be re-scanned.
    Scanned,
    /// Block ranges that have already been scanned. The nullifiers from this range were not mapped after scanning and
    /// spend detection to reduce memory consumption and/or storage for non-linear scanning. These nullifiers will need
    /// to be re-fetched for final spend detection when this range is the lowest unscanned range in the wallet's list
    /// of scan ranges.
    ScannedWithoutMapping,
    /// Block ranges to be scanned to advance the fully-scanned height.
    Historic,
    /// Block ranges adjacent to heights at which the user opened the wallet.
    OpenAdjacent,
    /// Blocks that must be scanned to complete note commitment tree shards adjacent to found notes.
    FoundNote,
    /// Blocks that must be scanned to complete the latest note commitment tree shard.
    ChainTip,
    /// A previously scanned range that must be verified to check it is still in the
    /// main chain, has highest priority.
    Verify,
}

/// A range of blocks to be scanned, along with its associated priority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanRange {
    block_range: Range<BlockHeight>,
    priority: ScanPriority,
}

impl std::fmt::Display for ScanRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:?}({}..{})",
            self.priority, self.block_range.start, self.block_range.end,
        )
    }
}

impl ScanRange {
    /// Constructs a scan range from its constituent parts.
    #[must_use]
    pub fn from_parts(block_range: Range<BlockHeight>, priority: ScanPriority) -> Self {
        assert!(
            block_range.end >= block_range.start,
            "{block_range:?} is invalid for ScanRange({priority:?})",
        );
        ScanRange {
            block_range,
            priority,
        }
    }

    /// Returns the range of block heights to be scanned.
    #[must_use]
    pub fn block_range(&self) -> &Range<BlockHeight> {
        &self.block_range
    }

    /// Returns the priority with which the scan range should be scanned.
    #[must_use]
    pub fn priority(&self) -> ScanPriority {
        self.priority
    }

    /// Returns whether or not the scan range is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.block_range.is_empty()
    }

    /// Returns the number of blocks in the scan range.
    #[must_use]
    pub fn len(&self) -> usize {
        usize::try_from(u32::from(self.block_range.end) - u32::from(self.block_range.start))
            .expect("due to number of max blocks should always be valid usize")
    }

    /// Shifts the start of the block range to the right if `block_height >
    /// self.block_range().start`. Returns `None` if the resulting range would
    /// be empty (or the range was already empty).
    #[must_use]
    pub fn truncate_start(&self, block_height: BlockHeight) -> Option<Self> {
        if block_height >= self.block_range.end || self.is_empty() {
            None
        } else {
            Some(ScanRange {
                block_range: self.block_range.start.max(block_height)..self.block_range.end,
                priority: self.priority,
            })
        }
    }

    /// Shifts the end of the block range to the left if `block_height <
    /// self.block_range().end`. Returns `None` if the resulting range would
    /// be empty (or the range was already empty).
    #[must_use]
    pub fn truncate_end(&self, block_height: BlockHeight) -> Option<Self> {
        if block_height <= self.block_range.start || self.is_empty() {
            None
        } else {
            Some(ScanRange {
                block_range: self.block_range.start..self.block_range.end.min(block_height),
                priority: self.priority,
            })
        }
    }

    /// Splits this scan range at the specified height, such that the provided height becomes the
    /// end of the first range returned and the start of the second. Returns `None` if
    /// `p <= self.block_range().start || p >= self.block_range().end`.
    #[must_use]
    pub fn split_at(&self, p: BlockHeight) -> Option<(Self, Self)> {
        (p > self.block_range.start && p < self.block_range.end).then_some((
            ScanRange {
                block_range: self.block_range.start..p,
                priority: self.priority,
            },
            ScanRange {
                block_range: p..self.block_range.end,
                priority: self.priority,
            },
        ))
    }
}

/// Syncs a wallet to the latest state of the blockchain.
///
/// `sync_mode` is intended to be stored in a struct that owns the wallet(s) (i.e. lightclient) and has a non-atomic
/// counterpart [`crate::wallet::SyncMode`]. The sync engine will set the `sync_mode` to `Running` at the start of sync.
/// However, the consumer is required to set the `sync_mode` back to `NotRunning` when sync is succussful or returns an
/// error. This allows more flexibility and safety with sync task handles etc.
/// `sync_mode` may also be set to `Paused` externally to pause scanning so the wallet lock can be acquired multiple
/// times in quick sucession without the sync engine interrupting.
/// Set `sync_mode` back to `Running` to resume scanning.
/// Set `sync_mode` to `Shutdown` to stop the sync process.
///
/// `events` announces committed sync events to subscribers as they happen (see [`crate::events`]).
/// Build one with [`SyncEmitter::new`], typically sized by
/// [`crate::config::SyncConfig::event_channel_capacity`].
pub async fn sync<C, P, W>(
    client: C,
    consensus_parameters: &P,
    wallet: Arc<RwLock<W>>,
    sync_mode: Arc<AtomicU8>,
    config: SyncConfig,
    events: SyncEmitter,
) -> Result<SyncResult, SyncError<W::Error>>
where
    C: Clone + Indexer + TransparentIndexer + Sync + Send + 'static,
    P: consensus::Parameters + Sync + Send + 'static,
    W: SyncWallet
        + SyncBlocks
        + SyncTransactions
        + SyncNullifiers
        + SyncOutPoints
        + SyncShardTrees
        + Send,
{
    let mut sync_mode_enum = SyncMode::from_atomic_u8(sync_mode.clone())?;
    if sync_mode_enum == SyncMode::NotRunning {
        sync_mode_enum = SyncMode::Running;
        sync_mode.store(sync_mode_enum as u8, atomic::Ordering::Release);
    } else {
        return Err(SyncModeError::SyncAlreadyRunning.into());
    }

    tracing::info!("Starting sync...");

    // create channel for sending fetch requests and launch fetcher task
    let (fetch_request_sender, fetch_request_receiver) = mpsc::unbounded_channel();
    let client_clone = client.clone();
    let fetcher_handle =
        tokio::spawn(
            async move { client::fetch::fetch(fetch_request_receiver, client_clone).await },
        );

    // create channel for receiving mempool transactions and launch mempool monitor
    let (mempool_transaction_sender, mut mempool_transaction_receiver) = mpsc::channel(100);
    let shutdown_mempool = Arc::new(AtomicBool::new(false));
    let shutdown_mempool_clone = shutdown_mempool.clone();
    let unprocessed_mempool_transactions_count = Arc::new(AtomicU8::new(0));
    let unprocessed_mempool_transactions_count_clone =
        unprocessed_mempool_transactions_count.clone();
    // dedupes `TipMoved` between the sync loop and the mempool monitor
    let tip_cell = Arc::new(AtomicU64::new(0));
    let mempool_events = events.clone();
    let mempool_tip_cell = tip_cell.clone();
    let mempool_handle = tokio::spawn(async move {
        mempool_monitor(
            client,
            mempool_transaction_sender,
            unprocessed_mempool_transactions_count_clone,
            shutdown_mempool_clone,
            mempool_events,
            mempool_tip_cell,
        )
        .await
    });

    // pre-scan initialisation
    let mut wallet_guard = wallet.write().await;

    let chain_height = client::get_chain_height(fetch_request_sender.clone()).await?;
    if chain_height == 0.into() {
        return Err(SyncError::ServerError(ServerError::GenesisBlockOnly));
    }
    let last_known_chain_height =
        checked_wallet_height(&mut *wallet_guard, chain_height, consensus_parameters)?;

    let ufvks = wallet_guard
        .get_unified_full_viewing_keys()
        .map_err(SyncError::WalletError)?;

    #[cfg(not(feature = "darkside_test"))]
    transparent::update_addresses_and_scan_targets(
        consensus_parameters,
        &mut *wallet_guard,
        fetch_request_sender.clone(),
        &ufvks,
        last_known_chain_height,
        chain_height,
        config.transparent_address_discovery,
    )
    .await?;

    #[cfg(not(feature = "darkside_test"))]
    update_subtree_roots(
        consensus_parameters,
        fetch_request_sender.clone(),
        &mut *wallet_guard,
    )
    .await?;

    add_initial_frontier(
        consensus_parameters,
        fetch_request_sender.clone(),
        &mut *wallet_guard,
    )
    .await?;

    let initial_reorg_detection_start_height = state::update_scan_ranges(
        consensus_parameters,
        fetch_request_sender.clone(),
        last_known_chain_height,
        chain_height,
        &mut *wallet_guard,
    )
    .await?;

    state::set_initial_state(
        consensus_parameters,
        fetch_request_sender.clone(),
        &mut *wallet_guard,
        chain_height,
    )
    .await?;

    expire_transactions(&mut *wallet_guard)?;

    let sync_state = wallet_guard
        .get_sync_state()
        .map_err(SyncError::WalletError)?;
    let birthday = sync_state
        .wallet_birthday()
        .expect("scan ranges must be non-empty after initialisation");
    let initial_sync_state = &sync_state.initial_sync_state;
    let wallet_tree_bounds = &initial_sync_state.wallet_tree_bounds;
    events.emit(SyncEvent::SessionStarted {
        sync_start_height: initial_sync_state.sync_start_height,
        birthday,
        tip: chain_height,
        total_sapling_outputs: wallet_tree_bounds.sapling_final_tree_size
            - wallet_tree_bounds.sapling_initial_tree_size,
        total_orchard_outputs: wallet_tree_bounds.orchard_final_tree_size
            - wallet_tree_bounds.orchard_initial_tree_size,
        already_scanned_sapling_outputs: initial_sync_state.previously_scanned_sapling_outputs,
        already_scanned_orchard_outputs: initial_sync_state.previously_scanned_orchard_outputs,
        already_scanned_blocks: initial_sync_state.previously_scanned_blocks,
    });
    emit_tip_if_advanced(&events, &tip_cell, chain_height);

    drop(wallet_guard);

    // create channel for receiving scan results and launch scanner
    let (scan_results_sender, mut scan_results_receiver) = mpsc::unbounded_channel();
    let mut scanner = Scanner::new(
        consensus_parameters.clone(),
        scan_results_sender,
        fetch_request_sender.clone(),
        ufvks.clone(),
    );
    scanner.launch(config.performance_level);

    // TODO: implement an option for continuous scanning where it doesnt exit when complete

    let mut nullifier_map_limit_exceeded = false;
    let mut interval = tokio::time::interval(Duration::from_millis(50));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            Some((scan_range, scan_results)) = scan_results_receiver.recv() => {
                let mut wallet_guard = wallet.write().await;
                let scan_events = process_scan_results(
                    consensus_parameters,
                    &mut *wallet_guard,
                    fetch_request_sender.clone(),
                    &ufvks,
                    scan_range,
                    scan_results,
                    initial_reorg_detection_start_height,
                    config.performance_level,
                    &mut nullifier_map_limit_exceeded,
                )
                .await?;
                wallet_guard.set_save_flag().map_err(SyncError::WalletError)?;
                // release first, then announce, so the send never overlaps the wallet lock
                drop(wallet_guard);
                events.emit_all(scan_events);
            }

            Some(raw_transaction) = mempool_transaction_receiver.recv() => {
                let mut wallet_guard = wallet.write().await;
                let discovered = process_mempool_transaction(
                    consensus_parameters,
                    &ufvks,
                    &mut *wallet_guard,
                    raw_transaction,
                )
                .await?;
                unprocessed_mempool_transactions_count.fetch_sub(1, atomic::Ordering::Release);
                drop(wallet_guard);
                if let Some(event) = discovered {
                    events.emit(event);
                }
            }

            _update_scanner = interval.tick() => {
                sync_mode_enum = SyncMode::from_atomic_u8(sync_mode.clone())?;
                match sync_mode_enum {
                    SyncMode::Paused => {
                        let mut pause_interval = tokio::time::interval(Duration::from_secs(1));
                        pause_interval.tick().await;
                        while sync_mode_enum == SyncMode::Paused {
                            pause_interval.tick().await;
                            sync_mode_enum = SyncMode::from_atomic_u8(sync_mode.clone())?;
                        }
                    },
                    SyncMode::Shutdown => {
                        let mut wallet_guard = wallet.write().await;
                        let sync_status = match sync_status(&*wallet_guard).await {
                            Ok(status) => status,
                            Err(SyncStatusError::WalletError(e)) => {
                                return Err(SyncError::WalletError(e));
                            }
                            Err(SyncStatusError::NoSyncData) => {
                                panic!("sync data must exist!");
                            }
                        };
                        let initial_sync_state = wallet_guard
                            .get_sync_state()
                            .map_err(SyncError::WalletError)?
                            .initial_sync_state
                            .clone();
                        wallet_guard
                            .set_save_flag()
                            .map_err(SyncError::WalletError)?;
                        drop(wallet_guard);
                        mempool_handle.abort();
                        fetcher_handle.abort();
                        tracing::info!("Sync successfully shutdown.");

                        return Ok(build_sync_result(sync_status, &initial_sync_state));
                    }
                    SyncMode::Running => (),
                    SyncMode::NotRunning => {
                        panic!("sync mode should not be manually set to NotRunning!");
                    },
                }

                scanner.update(&mut *wallet.write().await, shutdown_mempool.clone(), nullifier_map_limit_exceeded).await?;

                if matches!(scanner.state, ScannerState::Shutdown) {
                    // wait for mempool monitor to receive mempool transactions
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    if is_shutdown(&scanner, unprocessed_mempool_transactions_count.clone())
                    {
                        tracing::info!("Sync successfully shutdown.");
                        break;
                    }
                }
            }
        }
    }

    let mut wallet_guard = wallet.write().await;
    let sync_status = match sync_status(&*wallet_guard).await {
        Ok(status) => status,
        Err(SyncStatusError::WalletError(e)) => {
            return Err(SyncError::WalletError(e));
        }
        Err(SyncStatusError::NoSyncData) => {
            panic!("sync data must exist!");
        }
    };
    let initial_sync_state = wallet_guard
        .get_sync_state()
        .map_err(SyncError::WalletError)?
        .initial_sync_state
        .clone();
    // once sync is complete, all nullifiers will have been re-fetched so this note metadata can be discarded.
    for transaction in wallet_guard
        .get_wallet_transactions_mut()
        .map_err(SyncError::WalletError)?
        .values_mut()
    {
        for note in transaction.sapling_notes.as_mut_slice() {
            note.refetch_nullifier_ranges = Vec::new();
        }
        for note in transaction.orchard_notes.as_mut_slice() {
            note.refetch_nullifier_ranges = Vec::new();
        }
    }
    wallet_guard
        .set_save_flag()
        .map_err(SyncError::WalletError)?;

    drop(wallet_guard);
    drop(scanner);
    drop(fetch_request_sender);

    match mempool_handle.await.expect("task panicked") {
        Ok(()) => (),
        Err(e @ MempoolError::ShutdownWithoutStream) => tracing::warn!("{e}"),
        Err(e) => return Err(e.into()),
    }
    fetcher_handle.await.expect("task panicked");

    Ok(build_sync_result(sync_status, &initial_sync_state))
}

/// Builds a [`self::SyncResult`], recomputing this session's counts from the cumulative scanned
/// counts and the pre-session baseline.
fn build_sync_result(sync_status: SyncStatus, initial_sync_state: &InitialSyncState) -> SyncResult {
    SyncResult {
        sync_start_height: sync_status.sync_start_height,
        sync_end_height: (sync_status
            .scan_ranges
            .last()
            .expect("should be non-empty after syncing")
            .block_range()
            .end
            - 1),
        blocks_scanned: sync_status.total_blocks_scanned
            - initial_sync_state.previously_scanned_blocks,
        sapling_outputs_scanned: sync_status.total_sapling_outputs_scanned
            - initial_sync_state.previously_scanned_sapling_outputs,
        orchard_outputs_scanned: sync_status.total_orchard_outputs_scanned
            - initial_sync_state.previously_scanned_orchard_outputs,
    }
}

/// This ensures that the wallet height used to calculate the lower bound for scan range creation is valid.
/// The comparison takes two input heights and uses several constants to select the correct height.
///
/// The input parameter heights are:
///
///   (1) chain_height:
///       * the best block-height reported by the proxy (zainod or lwd)
///   (2) last_known_chain_height
///       * the last max height the wallet recorded from earlier scans
///
/// The constants are:
///   (1) MAX_REORG_ALLOWANCE:
///       * the maximum number of blocks the wallet can truncate during re-org detection
///   (2) Sapling Activation Height:
///       * the lower bound on the wallet birthday
fn checked_wallet_height<W, P>(
    wallet: &mut W,
    chain_height: BlockHeight,
    consensus_parameters: &P,
) -> Result<BlockHeight, SyncError<W::Error>>
where
    W: SyncBlocks + SyncTransactions + SyncNullifiers + SyncOutPoints + SyncShardTrees,
    P: zcash_protocol::consensus::Parameters,
{
    let sync_state = wallet.get_sync_state().map_err(SyncError::WalletError)?;
    if let Some(last_known_chain_height) = sync_state.last_known_chain_height() {
        if last_known_chain_height > chain_height {
            if last_known_chain_height - chain_height >= MAX_REORG_ALLOWANCE {
                // There's a human attention requiring problem, the wallet supplied
                // last_known_chain_height is more than MAX_REORG_ALLOWANCE **above**
                // the proxy's reported height.
                return Err(SyncError::ChainError(
                    u32::from(last_known_chain_height),
                    MAX_REORG_ALLOWANCE,
                    u32::from(chain_height),
                ));
            }
            // The wallet reported height is above the current proxy height
            // reset to the proxy height.
            truncate_wallet_data(wallet, chain_height)?;
            truncate_scan_ranges(
                chain_height,
                wallet
                    .get_sync_state_mut()
                    .map_err(SyncError::WalletError)?,
            );
            return Ok(chain_height);
        }
        // The last wallet reported height is equal or below the proxy height.
        Ok(last_known_chain_height)
    } else {
        // This is the wallet's first sync. Use [birthday - 1] as wallet height.
        let sapling_activation_height = consensus_parameters
            .activation_height(consensus::NetworkUpgrade::Sapling)
            .expect("sapling activation height should always return Some");
        let birthday = wallet.get_birthday().map_err(SyncError::WalletError)?;
        if birthday > chain_height {
            // Human attention requiring error, a birthday *above* the proxy reported
            // chain height has been provided.
            return Err(SyncError::ChainError(
                u32::from(birthday),
                MAX_REORG_ALLOWANCE,
                u32::from(chain_height),
            ));
        } else if birthday < sapling_activation_height {
            return Err(SyncError::BirthdayBelowSapling(
                u32::from(birthday),
                u32::from(sapling_activation_height),
            ));
        }

        Ok(birthday - 1)
    }
}

/// Creates a [`self::SyncStatus`] from the wallet's current [`crate::wallet::SyncState`].
///
/// Returns raw data only: scan range coverage and cumulative scanned counts. Intended for
/// reconciliation after a lagged event stream, reconnect, or wake from suspend. For live
/// observation, subscribe to the [`crate::events`] stream instead of polling this under the
/// wallet lock.
pub async fn sync_status<W>(wallet: &W) -> Result<SyncStatus, SyncStatusError<W::Error>>
where
    W: SyncWallet + SyncBlocks,
{
    let (total_sapling_outputs_scanned, total_orchard_outputs_scanned) =
        state::calculate_scanned_outputs(wallet).map_err(SyncStatusError::WalletError)?;

    let sync_state = wallet
        .get_sync_state()
        .map_err(SyncStatusError::WalletError)?;

    Ok(SyncStatus {
        scan_ranges: sync_state.scan_ranges.clone(),
        sync_start_height: sync_state.initial_sync_state.sync_start_height,
        total_blocks_scanned: state::calculate_scanned_blocks(sync_state),
        total_sapling_outputs_scanned,
        total_orchard_outputs_scanned,
    })
}

/// Scans a pending `transaction` of a given `status`, adding to the wallet and updating output spend statuses.
///
/// Used both internally for scanning mempool transactions and externally for scanning calculated and transmitted
/// transactions during send.
///
/// Panics if `status` is of `Confirmed` variant.
pub fn scan_pending_transaction<W>(
    consensus_parameters: &impl consensus::Parameters,
    ufvks: &HashMap<AccountId, UnifiedFullViewingKey>,
    wallet: &mut W,
    transaction: Transaction,
    status: ConfirmationStatus,
    datetime: u32,
) -> Result<(), SyncError<W::Error>>
where
    W: SyncWallet + SyncBlocks + SyncTransactions + SyncNullifiers + SyncOutPoints + SyncShardTrees,
{
    if matches!(status, ConfirmationStatus::Confirmed(_)) {
        panic!("this fn is for unconfirmed transactions only");
    }

    let mut pending_transaction_nullifiers = NullifierMap::new();
    let mut pending_transaction_outpoints = BTreeMap::new();
    let transparent_addresses: HashMap<String, TransparentAddressId> = wallet
        .get_transparent_addresses()
        .map_err(SyncError::WalletError)?
        .iter()
        .map(|(id, address)| (address.clone(), *id))
        .collect();
    let pending_transaction = scan_transaction(
        consensus_parameters,
        ufvks,
        transaction.txid(),
        transaction,
        status,
        None,
        &mut pending_transaction_nullifiers,
        &mut pending_transaction_outpoints,
        &transparent_addresses,
        datetime,
    )?;

    let wallet_transactions = wallet
        .get_wallet_transactions()
        .map_err(SyncError::WalletError)?;
    let transparent_output_ids = spend::collect_transparent_output_ids(wallet_transactions);
    let transparent_spend_scan_targets = spend::detect_transparent_spends(
        &mut pending_transaction_outpoints,
        transparent_output_ids,
    );
    let (sapling_derived_nullifiers, orchard_derived_nullifiers) =
        spend::collect_derived_nullifiers(wallet_transactions);
    let (sapling_spend_scan_targets, orchard_spend_scan_targets) = spend::detect_shielded_spends(
        &mut pending_transaction_nullifiers,
        sapling_derived_nullifiers,
        orchard_derived_nullifiers,
    );

    // return if transaction is not relevant to the wallet
    if pending_transaction.transparent_coins().is_empty()
        && pending_transaction.sapling_notes().is_empty()
        && pending_transaction.orchard_notes().is_empty()
        && pending_transaction.outgoing_orchard_notes().is_empty()
        && pending_transaction.outgoing_sapling_notes().is_empty()
        && transparent_spend_scan_targets.is_empty()
        && sapling_spend_scan_targets.is_empty()
        && orchard_spend_scan_targets.is_empty()
    {
        return Ok(());
    }

    wallet
        .insert_wallet_transaction(pending_transaction)
        .map_err(SyncError::WalletError)?;
    spend::update_spent_coins(
        wallet
            .get_wallet_transactions_mut()
            .map_err(SyncError::WalletError)?,
        transparent_spend_scan_targets,
    );
    spend::update_spent_notes(
        wallet,
        sapling_spend_scan_targets,
        orchard_spend_scan_targets,
        false,
    )
    .map_err(SyncError::WalletError)?;

    Ok(())
}

/// API for targetted scanning.
///
/// Allows `scan_targets` to be added externally to the wallet's `sync_state` and be prioritised for scanning. Each
/// scan target must include the block height which will be used to prioritise the block range containing the note
/// commitments to the surrounding orchard shard(s). If the block height is pre-orchard then the surrounding sapling
/// shard(s) will be prioritised instead. The txid in each scan target may be omitted and set to [0u8; 32] in order to
/// prioritise the surrounding blocks for scanning but be ignored when fetching specific relevant transactions to the
/// wallet. However, in the case where a relevant spending transaction at a given height contains no decryptable
/// incoming notes (change), only the nullifier will be mapped and this transaction will be scanned when the
/// transaction containing the spent notes is scanned instead.
pub fn add_scan_targets(sync_state: &mut SyncState, scan_targets: &[ScanTarget]) {
    for scan_target in scan_targets {
        sync_state.scan_targets.insert(*scan_target);
    }
}

/// Resets the spending transaction field of all outputs that were previously spent but became unspent due to a
/// spending transactions becoming invalid.
///
/// `invalid_txids` are the id's of the invalidated spending transactions. Any outputs in the `wallet_transactions`
/// matching these spending transactions will be reset back to `None`.
pub fn reset_spends(
    wallet_transactions: &mut HashMap<TxId, WalletTransaction>,
    invalid_txids: Vec<TxId>,
) {
    wallet_transactions
        .values_mut()
        .flat_map(|transaction| transaction.orchard_notes_mut())
        .filter(|output| {
            output
                .spending_transaction
                .is_some_and(|spending_txid| invalid_txids.contains(&spending_txid))
        })
        .for_each(|output| {
            output.set_spending_transaction(None);
        });
    wallet_transactions
        .values_mut()
        .flat_map(|transaction| transaction.sapling_notes_mut())
        .filter(|output| {
            output
                .spending_transaction
                .is_some_and(|spending_txid| invalid_txids.contains(&spending_txid))
        })
        .for_each(|output| {
            output.set_spending_transaction(None);
        });
    wallet_transactions
        .values_mut()
        .flat_map(|transaction| transaction.transparent_coins_mut())
        .filter(|output| {
            output
                .spending_transaction
                .is_some_and(|spending_txid| invalid_txids.contains(&spending_txid))
        })
        .for_each(|output| {
            output.set_spending_transaction(None);
        });
}

/// Sets transactions associated with list of `failed_txids` in `wallet_transactions` to `Failed` status.
///
/// Sets the `spending_transaction` fields of any outputs spent in these transactions to `None`.
pub fn set_transactions_failed(
    wallet_transactions: &mut HashMap<TxId, WalletTransaction>,
    failed_txids: Vec<TxId>,
) {
    for failed_txid in failed_txids.iter() {
        if let Some(transaction) = wallet_transactions.get_mut(failed_txid) {
            let height = transaction.status().get_height();
            transaction.update_status(
                ConfirmationStatus::Failed(height),
                SystemTime::now()
                    .duration_since(SystemTime::UNIX_EPOCH)
                    .expect("infalliable for such long time periods")
                    .as_secs() as u32,
            );
        }
    }
    reset_spends(wallet_transactions, failed_txids);
}

/// Returns true if the scanner and mempool are shutdown.
fn is_shutdown<P>(
    scanner: &Scanner<P>,
    mempool_unprocessed_transactions_count: Arc<AtomicU8>,
) -> bool
where
    P: consensus::Parameters + Sync + Send + 'static,
{
    scanner.worker_poolsize() == 0
        && mempool_unprocessed_transactions_count.load(atomic::Ordering::Acquire) == 0
}

/// Scan post-processing
///
/// Returns the [`crate::events::SyncEvent`]s describing what this call committed, for the caller
/// to emit once the wallet lock is released. Events are built on committed paths only, so an event
/// is never returned for data a reorg in the same call truncates.
#[allow(clippy::too_many_arguments)]
async fn process_scan_results<W>(
    consensus_parameters: &impl consensus::Parameters,
    wallet: &mut W,
    fetch_request_sender: mpsc::UnboundedSender<FetchRequest>,
    ufvks: &HashMap<AccountId, UnifiedFullViewingKey>,
    scan_range: ScanRange,
    scan_results: Result<ScanResults, ScanError>,
    initial_reorg_detection_start_height: BlockHeight,
    performance_level: PerformanceLevel,
    nullifier_map_limit_exceeded: &mut bool,
) -> Result<Vec<SyncEvent>, SyncError<W::Error>>
where
    W: SyncWallet
        + SyncBlocks
        + SyncTransactions
        + SyncNullifiers
        + SyncOutPoints
        + SyncShardTrees
        + Send,
{
    let mut events = Vec::new();
    match scan_results {
        Ok(results) => {
            let ScanResults {
                mut nullifiers,
                mut outpoints,
                scanned_blocks,
                wallet_transactions,
                sapling_located_trees,
                orchard_located_trees,
            } = results;

            if scan_range.priority() == ScanPriority::ScannedWithoutMapping {
                // no events on this path: the range's blocks and outputs were already announced
                // when first scanned, before its nullifiers were discarded.

                // add missing block bounds in the case that nullifier batch limit was reached and the fetch nullifier
                // scan range was split.
                let full_refetching_nullifiers_range = wallet
                    .get_sync_state()
                    .map_err(SyncError::WalletError)?
                    .scan_ranges
                    .iter()
                    .find(|&wallet_scan_range| {
                        wallet_scan_range
                            .block_range()
                            .contains(&scan_range.block_range().start)
                            && wallet_scan_range
                                .block_range()
                                .contains(&(scan_range.block_range().end - 1))
                    })
                    .expect("wallet scan range containing scan range should exist!");
                if scan_range.block_range().start
                    != full_refetching_nullifiers_range.block_range().start
                    || scan_range.block_range().end
                        != full_refetching_nullifiers_range.block_range().end
                {
                    let mut missing_block_bounds = BTreeMap::new();
                    for block_bound in [
                        scan_range.block_range().start - 1,
                        scan_range.block_range().start,
                        scan_range.block_range().end - 1,
                        scan_range.block_range().end,
                    ] {
                        if block_bound < full_refetching_nullifiers_range.block_range().start
                            || block_bound >= full_refetching_nullifiers_range.block_range().end
                        {
                            continue;
                        }
                        if wallet.get_wallet_block(block_bound).is_err() {
                            missing_block_bounds.insert(
                                block_bound,
                                WalletBlock::from_compact_block(
                                    consensus_parameters,
                                    fetch_request_sender.clone(),
                                    &client::get_compact_block(
                                        fetch_request_sender.clone(),
                                        block_bound,
                                    )
                                    .await?,
                                )
                                .await?,
                            );
                        }
                    }
                    if !missing_block_bounds.is_empty() {
                        wallet
                            .append_wallet_blocks(missing_block_bounds)
                            .map_err(SyncError::WalletError)?;
                    }
                }

                let first_unscanned_range = wallet
                    .get_sync_state()
                    .map_err(SyncError::WalletError)?
                    .scan_ranges
                    .iter()
                    .find(|scan_range| scan_range.priority() != ScanPriority::Scanned)
                    .expect("the scan range being processed is not yet set to scanned so at least one unscanned range must exist");
                if !first_unscanned_range
                    .block_range()
                    .contains(&scan_range.block_range().start)
                    || !first_unscanned_range
                        .block_range()
                        .contains(&(scan_range.block_range().end - 1))
                {
                    // in this rare edge case, a scanned `ScannedWithoutMapping` range was the highest priority yet it was not the first unscanned range so it must be discarded to avoid missing spends

                    // reset scan range from `RefetchingNullifiers` to `ScannedWithoutMapping`
                    state::reset_refetching_nullifiers_scan_range(
                        wallet
                            .get_sync_state_mut()
                            .map_err(SyncError::WalletError)?,
                        scan_range.block_range().clone(),
                    );
                    tracing::debug!(
                        "Nullifiers discarded and will be re-fetched to avoid missing spends."
                    );

                    return Ok(events);
                }

                spend::update_shielded_spends(
                    consensus_parameters,
                    wallet,
                    fetch_request_sender.clone(),
                    ufvks,
                    &scanned_blocks,
                    Some(&mut nullifiers),
                )
                .await?;

                state::set_scanned_scan_range(
                    wallet
                        .get_sync_state_mut()
                        .map_err(SyncError::WalletError)?,
                    scan_range.block_range().clone(),
                    true, // NOTE: although nullifiers are not actually added to the wallet's nullifier map for efficiency, there is effectively no difference as spends are still updated using the `additional_nullifier_map` and would be removed on the following cleanup (`remove_irrelevant_data`) due to `ScannedWithoutMapping` ranges always being the first non-scanned range and therefore always raise the wallet's fully scanned height after processing.
                );
            } else {
                // output counts are tree-size deltas summed over the batch's scanned blocks,
                // computed before `scanned_blocks` is moved into the wallet.
                let (sapling_outputs, orchard_outputs) =
                    scanned_blocks.values().fold((0u32, 0u32), |acc, block| {
                        let tree_bounds = block.tree_bounds();
                        (
                            acc.0
                                + (tree_bounds.sapling_final_tree_size
                                    - tree_bounds.sapling_initial_tree_size),
                            acc.1
                                + (tree_bounds.orchard_final_tree_size
                                    - tree_bounds.orchard_initial_tree_size),
                        )
                    });
                events.reserve(wallet_transactions.len() + 1);
                events.push(SyncEvent::RangeScanned {
                    range: scan_range.block_range().clone(),
                    priority: scan_range.priority(),
                    sapling_outputs,
                    orchard_outputs,
                });
                for (txid, transaction) in &wallet_transactions {
                    events.push(SyncEvent::TxDiscovered {
                        txid: *txid,
                        status: transaction.status(),
                    });
                }

                // nullifiers are not mapped if nullifier map size limit will be exceeded
                if !*nullifier_map_limit_exceeded {
                    let nullifier_map = wallet.get_nullifiers().map_err(SyncError::WalletError)?;
                    if max_nullifier_map_size(performance_level).is_some_and(|max| {
                        nullifier_map.orchard.len()
                            + nullifier_map.sapling.len()
                            + nullifiers.orchard.len()
                            + nullifiers.sapling.len()
                            > max
                    }) {
                        *nullifier_map_limit_exceeded = true;
                    }
                }
                let mut map_nullifiers = !*nullifier_map_limit_exceeded;

                // all transparent spend locations are known before scanning so there is no need to map outpoints from untargetted ranges.
                // outpoints of untargetted ranges will still be checked before being discarded.
                let map_outpoints = scan_range.priority() >= ScanPriority::FoundNote;

                // always map nullifiers if scanning the lowest range to be scanned for final spend detection.
                // this will set the range to `Scanned` (as oppose to `ScannedWithoutMapping`) and prevent immediate
                // re-fetching of the nullifiers in this range. these will be immediately cleared after cleanup so will not
                // have an impact on memory or wallet file size.
                // the selected range is not the lowest range to be scanned unless all ranges before it are scanned or
                // scanning.
                for query_scan_range in wallet
                    .get_sync_state()
                    .map_err(SyncError::WalletError)?
                    .scan_ranges()
                {
                    let scan_priority = query_scan_range.priority();
                    if scan_priority != ScanPriority::Scanned
                        && scan_priority != ScanPriority::Scanning
                        && scan_priority != ScanPriority::RefetchingNullifiers
                    {
                        break;
                    }

                    if scan_priority == ScanPriority::Scanning
                        && query_scan_range
                            .block_range()
                            .contains(&scan_range.block_range().start)
                        && query_scan_range
                            .block_range()
                            .contains(&(scan_range.block_range().end - 1))
                    {
                        map_nullifiers = true;
                        break;
                    }
                }

                update_wallet_data(
                    consensus_parameters,
                    wallet,
                    fetch_request_sender.clone(),
                    ufvks,
                    &scan_range,
                    if map_nullifiers {
                        Some(&mut nullifiers)
                    } else {
                        None
                    },
                    if map_outpoints {
                        Some(&mut outpoints)
                    } else {
                        None
                    },
                    wallet_transactions,
                    sapling_located_trees,
                    orchard_located_trees,
                )
                .await?;
                spend::update_transparent_spends(
                    wallet,
                    if map_outpoints {
                        None
                    } else {
                        Some(&mut outpoints)
                    },
                )
                .map_err(SyncError::WalletError)?;
                spend::update_shielded_spends(
                    consensus_parameters,
                    wallet,
                    fetch_request_sender,
                    ufvks,
                    &scanned_blocks,
                    if map_nullifiers {
                        None
                    } else {
                        Some(&mut nullifiers)
                    },
                )
                .await?;
                add_scanned_blocks(wallet, scanned_blocks, &scan_range)
                    .map_err(SyncError::WalletError)?;

                state::set_scanned_scan_range(
                    wallet
                        .get_sync_state_mut()
                        .map_err(SyncError::WalletError)?,
                    scan_range.block_range().clone(),
                    map_nullifiers,
                );
                state::merge_scan_ranges(
                    wallet
                        .get_sync_state_mut()
                        .map_err(SyncError::WalletError)?,
                    ScanPriority::ScannedWithoutMapping,
                );
            }

            state::merge_scan_ranges(
                wallet
                    .get_sync_state_mut()
                    .map_err(SyncError::WalletError)?,
                ScanPriority::Scanned,
            );
            remove_irrelevant_data(wallet).map_err(SyncError::WalletError)?;
            tracing::debug!("Scan results processed.");
        }
        Err(ScanError::ContinuityError(ContinuityError::HashDiscontinuity { height, .. })) => {
            tracing::warn!("Hash discontinuity detected before block {height}.");
            if height == scan_range.block_range().start
                && scan_range.priority() == ScanPriority::Verify
            {
                tracing::info!("Re-org detected.");
                let sync_state = wallet
                    .get_sync_state_mut()
                    .map_err(SyncError::WalletError)?;
                let last_known_chain_height = sync_state
                    .last_known_chain_height()
                    .expect("scan ranges should be non-empty in this scope");

                // reset scan range from `Scanning` to `Verify`
                state::set_scan_priority(
                    sync_state,
                    scan_range.block_range(),
                    ScanPriority::Verify,
                );

                // extend verification range to VERIFY_BLOCK_RANGE_SIZE blocks below current verification range
                let current_reorg_detection_start_height = state::set_verify_scan_range(
                    sync_state,
                    height - 1,
                    state::VerifyEnd::VerifyHighest,
                )
                .block_range()
                .start;
                state::merge_scan_ranges(sync_state, ScanPriority::Verify);

                if initial_reorg_detection_start_height - current_reorg_detection_start_height
                    > MAX_REORG_ALLOWANCE
                {
                    clear_wallet_data(wallet)?;

                    return Err(ServerError::ChainVerificationError.into());
                }

                truncate_wallet_data(wallet, current_reorg_detection_start_height - 1)?;
                events.push(SyncEvent::Reorg {
                    reverted_to: current_reorg_detection_start_height - 1,
                });

                state::set_initial_state(
                    consensus_parameters,
                    fetch_request_sender.clone(),
                    wallet,
                    last_known_chain_height,
                )
                .await?;
            } else {
                scan_results?;
            }
        }
        Err(e) => return Err(e.into()),
    }

    Ok(events)
}

/// Processes mempool transaction.
///
/// Scan the transaction and add to the wallet if relevant. Returns the
/// [`crate::events::SyncEvent::TxDiscovered`] to emit once the wallet lock is released, or `None`
/// if the transaction is not relevant to the wallet.
async fn process_mempool_transaction<W>(
    consensus_parameters: &impl consensus::Parameters,
    ufvks: &HashMap<AccountId, UnifiedFullViewingKey>,
    wallet: &mut W,
    raw_transaction: RawTransaction,
) -> Result<Option<SyncEvent>, SyncError<W::Error>>
where
    W: SyncWallet + SyncBlocks + SyncTransactions + SyncNullifiers + SyncOutPoints + SyncShardTrees,
{
    // does not use raw transaction height due to lightwalletd off-by-one bug and potential to be zero
    let mempool_height = wallet
        .get_sync_state()
        .map_err(SyncError::WalletError)?
        .last_known_chain_height()
        .expect("wallet height must exist after sync is initialised")
        + 1;

    let transaction = zcash_primitives::transaction::Transaction::read(
        &raw_transaction.data[..],
        consensus::BranchId::for_height(consensus_parameters, mempool_height),
    )
    .map_err(ServerError::InvalidTransaction)?;

    let txid = transaction.txid();
    let status = ConfirmationStatus::Mempool(mempool_height);

    tracing::debug!(
        "mempool received txid {} at height {}",
        txid,
        mempool_height
    );

    if let Some(tx) = wallet
        .get_wallet_transactions_mut()
        .map_err(SyncError::WalletError)?
        .get_mut(&txid)
    {
        tx.update_status(
            status,
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("infalliable for such long time periods")
                .as_secs() as u32,
        );

        return Ok(Some(SyncEvent::TxDiscovered { txid, status }));
    }

    scan_pending_transaction(
        consensus_parameters,
        ufvks,
        wallet,
        transaction,
        status,
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("infalliable for such long time periods")
            .as_secs() as u32,
    )?;

    // `scan_pending_transaction` only inserts relevant transactions
    Ok(wallet
        .get_wallet_transactions()
        .map_err(SyncError::WalletError)?
        .contains_key(&txid)
        .then_some(SyncEvent::TxDiscovered { txid, status }))
}

/// Removes wallet blocks, transactions, nullifiers, outpoints and shard tree data above the given `truncate_height`.
fn truncate_wallet_data<W>(
    wallet: &mut W,
    truncate_height: BlockHeight,
) -> Result<(), SyncError<W::Error>>
where
    W: SyncWallet + SyncBlocks + SyncTransactions + SyncNullifiers + SyncOutPoints + SyncShardTrees,
{
    let sync_state = wallet
        .get_sync_state_mut()
        .map_err(SyncError::WalletError)?;
    let highest_scanned_height = sync_state
        .highest_scanned_height()
        .expect("should be non-empty in this scope");
    let wallet_birthday = sync_state
        .wallet_birthday()
        .expect("should be non-empty in this scope");
    let checked_truncate_height = match truncate_height.cmp(&wallet_birthday) {
        std::cmp::Ordering::Greater | std::cmp::Ordering::Equal => truncate_height,
        std::cmp::Ordering::Less => consensus::H0,
    };

    if checked_truncate_height > highest_scanned_height {
        return Ok(());
    }

    wallet
        .truncate_wallet_blocks(checked_truncate_height)
        .map_err(SyncError::WalletError)?;
    wallet
        .truncate_wallet_transactions(checked_truncate_height)
        .map_err(SyncError::WalletError)?;
    wallet
        .truncate_nullifiers(checked_truncate_height)
        .map_err(SyncError::WalletError)?;
    wallet
        .truncate_outpoints(checked_truncate_height)
        .map_err(SyncError::WalletError)?;
    match wallet.truncate_shard_trees(checked_truncate_height) {
        Ok(_) => Ok(()),
        Err(SyncError::TruncationError(height, pooltype)) => {
            clear_wallet_data(wallet)?;

            Err(SyncError::TruncationError(height, pooltype))
        }
        Err(e) => Err(e),
    }?;

    Ok(())
}

fn clear_wallet_data<W>(wallet: &mut W) -> Result<(), SyncError<W::Error>>
where
    W: SyncWallet + SyncBlocks + SyncTransactions + SyncNullifiers + SyncOutPoints + SyncShardTrees,
{
    let scan_targets = wallet
        .get_wallet_transactions()
        .map_err(SyncError::WalletError)?
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
        .collect::<Vec<_>>();
    truncate_wallet_data(wallet, consensus::H0)?;
    truncate_scan_ranges(
        consensus::H0,
        wallet
            .get_sync_state_mut()
            .map_err(SyncError::WalletError)?,
    );
    wallet
        .get_wallet_transactions_mut()
        .map_err(SyncError::WalletError)?
        .clear();
    let sync_state = wallet
        .get_sync_state_mut()
        .map_err(SyncError::WalletError)?;
    add_scan_targets(sync_state, &scan_targets);
    wallet.set_save_flag().map_err(SyncError::WalletError)?;

    Ok(())
}

/// Updates the wallet with data from `scan_results`
#[allow(clippy::too_many_arguments)]
async fn update_wallet_data<W>(
    consensus_parameters: &impl consensus::Parameters,
    wallet: &mut W,
    fetch_request_sender: mpsc::UnboundedSender<FetchRequest>,
    ufvks: &HashMap<AccountId, UnifiedFullViewingKey>,
    scan_range: &ScanRange,
    nullifiers: Option<&mut NullifierMap>,
    outpoints: Option<&mut BTreeMap<OutputId, ScanTarget>>,
    mut transactions: HashMap<TxId, WalletTransaction>,
    sapling_located_trees: Vec<LocatedTreeData<sapling_crypto::Node>>,
    orchard_located_trees: Vec<LocatedTreeData<MerkleHashOrchard>>,
) -> Result<(), SyncError<W::Error>>
where
    W: SyncBlocks + SyncTransactions + SyncNullifiers + SyncOutPoints + SyncShardTrees + Send,
{
    let sync_state = wallet
        .get_sync_state_mut()
        .map_err(SyncError::WalletError)?;
    let highest_scanned_height = sync_state
        .highest_scanned_height()
        .expect("scan ranges should not be empty in this scope");
    for transaction in transactions.values() {
        state::update_found_note_shard_priority(
            consensus_parameters,
            sync_state,
            ShieldedProtocol::Sapling,
            transaction,
        );
        state::update_found_note_shard_priority(
            consensus_parameters,
            sync_state,
            ShieldedProtocol::Orchard,
            transaction,
        );
    }
    // add all block ranges of scan ranges with `ScannedWithoutMapping` or `RefetchingNullifiers` priority above the
    // current scan range to each note to track which ranges need the nullifiers to be re-fetched before the note is
    // known to be unspent (in addition to all other ranges above the notes height being `Scanned`,
    // `ScannedWithoutMapping` or `RefetchingNullifiers` priority). this information is necessary as these ranges have been scanned but the
    // nullifiers have been discarded so must be re-fetched. if ranges are scanned but the nullifiers are discarded
    // (set to `ScannedWithoutMapping` priority) *after* this note has been added to the wallet, this is sufficient to
    // know this note has not been spent, even if this range is not set to `Scanned` priority.
    let refetch_nullifier_ranges = {
        let block_ranges: Vec<Range<BlockHeight>> = sync_state
            .scan_ranges()
            .iter()
            .filter(|&scan_range| {
                scan_range.priority() == ScanPriority::ScannedWithoutMapping
                    || scan_range.priority() == ScanPriority::RefetchingNullifiers
            })
            .map(|scan_range| scan_range.block_range().clone())
            .collect();

        block_ranges
            [block_ranges.partition_point(|range| range.start < scan_range.block_range().end)..]
            .to_vec()
    };
    for transaction in transactions.values_mut() {
        for note in transaction.sapling_notes.as_mut_slice() {
            note.refetch_nullifier_ranges = refetch_nullifier_ranges.clone();
        }
        for note in transaction.orchard_notes.as_mut_slice() {
            note.refetch_nullifier_ranges = refetch_nullifier_ranges.clone();
        }
    }
    for transaction in transactions.values() {
        discover_unified_addresses(wallet, ufvks, transaction).map_err(SyncError::WalletError)?;
    }

    wallet
        .extend_wallet_transactions(transactions)
        .map_err(SyncError::WalletError)?;
    if let Some(nullifiers) = nullifiers {
        wallet
            .append_nullifiers(nullifiers)
            .map_err(SyncError::WalletError)?;
    }
    if let Some(outpoints) = outpoints {
        wallet
            .append_outpoints(outpoints)
            .map_err(SyncError::WalletError)?;
    }
    wallet
        .update_shard_trees(
            fetch_request_sender,
            scan_range,
            highest_scanned_height,
            sapling_located_trees,
            orchard_located_trees,
        )
        .await?;

    Ok(())
}

fn discover_unified_addresses<W>(
    wallet: &mut W,
    ufvks: &HashMap<AccountId, UnifiedFullViewingKey>,
    transaction: &WalletTransaction,
) -> Result<(), W::Error>
where
    W: SyncWallet,
{
    for note in transaction
        .orchard_notes()
        .iter()
        .filter(|&note| note.key_id().scope == zip32::Scope::External)
    {
        let ivk = ufvks
            .get(&note.key_id().account_id())
            .expect("ufvk must exist to decrypt this note")
            .orchard()
            .expect("fvk must exist to decrypt this note")
            .to_ivk(zip32::Scope::External);

        wallet.add_orchard_address(
            note.key_id().account_id(),
            note.note().recipient(),
            ivk.diversifier_index(&note.note().recipient())
                .expect("must be key used to create this address"),
        )?;
    }
    for note in transaction
        .sapling_notes()
        .iter()
        .filter(|&note| note.key_id().scope == zip32::Scope::External)
    {
        let ivk = ufvks
            .get(&note.key_id().account_id())
            .expect("ufvk must exist to decrypt this note")
            .sapling()
            .expect("fvk must exist to decrypt this note")
            .to_external_ivk();

        wallet.add_sapling_address(
            note.key_id().account_id(),
            note.note().recipient(),
            ivk.decrypt_diversifier(&note.note().recipient())
                .expect("must be key used to create this address"),
        )?;
    }

    Ok(())
}

fn remove_irrelevant_data<W>(wallet: &mut W) -> Result<(), W::Error>
where
    W: SyncWallet + SyncBlocks + SyncOutPoints + SyncNullifiers + SyncTransactions,
{
    let fully_scanned_height = wallet
        .get_sync_state()?
        .fully_scanned_height()
        .expect("scan ranges must be non-empty");

    wallet
        .get_outpoints_mut()?
        .retain(|_, scan_target| scan_target.block_height > fully_scanned_height);
    wallet
        .get_nullifiers_mut()?
        .sapling
        .retain(|_, scan_target| scan_target.block_height > fully_scanned_height);
    wallet
        .get_nullifiers_mut()?
        .orchard
        .retain(|_, scan_target| scan_target.block_height > fully_scanned_height);
    wallet
        .get_sync_state_mut()?
        .scan_targets
        .retain(|scan_target| scan_target.block_height > fully_scanned_height);
    remove_irrelevant_blocks(wallet)?;

    Ok(())
}

fn remove_irrelevant_blocks<W>(wallet: &mut W) -> Result<(), W::Error>
where
    W: SyncWallet + SyncBlocks + SyncTransactions,
{
    let sync_state = wallet.get_sync_state()?;
    let highest_scanned_height = sync_state
        .highest_scanned_height()
        .expect("should be non-empty");
    let scanned_range_bounds = sync_state
        .scan_ranges()
        .iter()
        .filter(|scan_range| {
            scan_range.priority() == ScanPriority::Scanned
                || scan_range.priority() == ScanPriority::ScannedWithoutMapping
                || scan_range.priority() == ScanPriority::RefetchingNullifiers
        })
        .flat_map(|scanned_range| {
            vec![
                scanned_range.block_range().start,
                scanned_range.block_range().end - 1,
            ]
        })
        .collect::<Vec<_>>();
    let wallet_transaction_heights = wallet
        .get_wallet_transactions()?
        .values()
        .filter_map(|tx| tx.status().get_confirmed_height())
        .collect::<Vec<_>>();

    wallet.get_wallet_blocks_mut()?.retain(|height, _| {
        *height >= highest_scanned_height.saturating_sub(MAX_REORG_ALLOWANCE)
            || scanned_range_bounds.contains(height)
            || wallet_transaction_heights.contains(height)
    });

    Ok(())
}

fn add_scanned_blocks<W>(
    wallet: &mut W,
    mut scanned_blocks: BTreeMap<BlockHeight, WalletBlock>,
    scan_range: &ScanRange,
) -> Result<(), W::Error>
where
    W: SyncWallet + SyncBlocks + SyncTransactions,
{
    let sync_state = wallet.get_sync_state()?;
    let highest_scanned_height = sync_state
        .highest_scanned_height()
        .expect("scan ranges must be non-empty");

    let wallet_transaction_heights = wallet
        .get_wallet_transactions()?
        .values()
        .filter_map(|tx| tx.status().get_confirmed_height())
        .collect::<Vec<_>>();

    scanned_blocks.retain(|height, _| {
        *height >= highest_scanned_height.saturating_sub(MAX_REORG_ALLOWANCE)
            || *height == scan_range.block_range().start
            || *height == scan_range.block_range().end - 1
            || wallet_transaction_heights.contains(height)
    });

    wallet.append_wallet_blocks(scanned_blocks)?;

    Ok(())
}

#[cfg(not(feature = "darkside_test"))]
async fn update_subtree_roots<W>(
    consensus_parameters: &impl consensus::Parameters,
    fetch_request_sender: mpsc::UnboundedSender<FetchRequest>,
    wallet: &mut W,
) -> Result<(), SyncError<W::Error>>
where
    W: SyncWallet + SyncShardTrees,
{
    let sapling_start_index = wallet
        .get_shard_trees()
        .map_err(SyncError::WalletError)?
        .sapling
        .store()
        .get_shard_roots()
        .expect("infallible")
        .len() as u32;
    let orchard_start_index = wallet
        .get_shard_trees()
        .map_err(SyncError::WalletError)?
        .orchard
        .store()
        .get_shard_roots()
        .expect("infallible")
        .len() as u32;
    let (sapling_subtree_roots, orchard_subtree_roots) = futures::join!(
        client::get_subtree_roots(fetch_request_sender.clone(), sapling_start_index, 0, 0),
        client::get_subtree_roots(fetch_request_sender, orchard_start_index, 1, 0)
    );

    let sapling_subtree_roots = sapling_subtree_roots?;
    let orchard_subtree_roots = orchard_subtree_roots?;

    let sync_state = wallet
        .get_sync_state_mut()
        .map_err(SyncError::WalletError)?;
    state::add_shard_ranges(
        consensus_parameters,
        ShieldedProtocol::Sapling,
        sync_state,
        &sapling_subtree_roots,
    );
    state::add_shard_ranges(
        consensus_parameters,
        ShieldedProtocol::Orchard,
        sync_state,
        &orchard_subtree_roots,
    );

    let shard_trees = wallet
        .get_shard_trees_mut()
        .map_err(SyncError::WalletError)?;
    witness::add_subtree_roots(
        sapling_start_index as usize,
        sapling_subtree_roots,
        &mut shard_trees.sapling,
    )?;
    witness::add_subtree_roots(
        orchard_start_index as usize,
        orchard_subtree_roots,
        &mut shard_trees.orchard,
    )?;

    Ok(())
}

async fn add_initial_frontier<W>(
    consensus_parameters: &impl consensus::Parameters,
    fetch_request_sender: mpsc::UnboundedSender<FetchRequest>,
    wallet: &mut W,
) -> Result<(), SyncError<W::Error>>
where
    W: SyncWallet + SyncShardTrees,
{
    let birthday = wallet.get_birthday().map_err(SyncError::WalletError)?;
    if birthday
        == consensus_parameters
            .activation_height(consensus::NetworkUpgrade::Sapling)
            .expect("sapling activation height should always return Some")
    {
        return Ok(());
    }

    // if the shard store only contains the first checkpoint added on initialisation, add frontiers to complete the
    // shard trees.
    let shard_trees = wallet
        .get_shard_trees_mut()
        .map_err(SyncError::WalletError)?;
    if shard_trees
        .sapling
        .store()
        .checkpoint_count()
        .expect("infallible")
        == 1
    {
        let frontiers = client::get_frontiers(fetch_request_sender, birthday).await?;
        shard_trees
            .sapling
            .insert_frontier(
                frontiers.final_sapling_tree().clone(),
                Retention::Checkpoint {
                    id: birthday,
                    marking: Marking::None,
                },
            )
            .expect("infallible");
        shard_trees
            .orchard
            .insert_frontier(
                frontiers.final_orchard_tree().clone(),
                Retention::Checkpoint {
                    id: birthday,
                    marking: Marking::None,
                },
            )
            .expect("infallible");
    }

    Ok(())
}

/// Sets up mempool stream.
///
/// If there is some raw transaction, send to be scanned.
/// If the mempool stream message is `None` (a block was mined) or the request failed, setup a new mempool stream.
async fn mempool_monitor<C>(
    mut client: C,
    mempool_transaction_sender: mpsc::Sender<RawTransaction>,
    unprocessed_transactions_count: Arc<AtomicU8>,
    shutdown_mempool: Arc<AtomicBool>,
    events: SyncEmitter,
    tip_cell: Arc<AtomicU64>,
) -> Result<(), MempoolError>
where
    C: Clone + Indexer + TransparentIndexer + Sync + Send + 'static,
{
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    'main: loop {
        let response =
            client::get_mempool_transaction_stream(&mut client, shutdown_mempool.clone()).await;

        match response {
            Ok(mut mempool_stream) => {
                interval.reset();
                loop {
                    tokio::select! {
                        mempool_stream_message = mempool_stream.message() => {
                            match mempool_stream_message.unwrap_or(None) {
                                Some(raw_transaction) => {
                                     let _ignore_error = mempool_transaction_sender
                                        .send(raw_transaction)
                                        .await;
                                    unprocessed_transactions_count.fetch_add(1, atomic::Ordering::Release);
                                }
                                None => {
                                    // the stream resets when a block is mined, making this the
                                    // lowest-latency tip signal. fetched on an idle path.
                                    match client.get_latest_block(Duration::from_secs(10)).await {
                                        Ok(block_id) => emit_tip_if_advanced(
                                            &events,
                                            &tip_cell,
                                            BlockHeight::from_u32(block_id.height as u32),
                                        ),
                                        Err(e) => tracing::warn!(
                                            "Failed to fetch chain tip after mempool stream reset: {e}."
                                        ),
                                    }
                                    continue 'main;
                                }
                            }

                        }

                        _ = interval.tick() => {
                            if shutdown_mempool.load(atomic::Ordering::Acquire) {
                                break 'main;
                            }
                        }
                    }
                }
            }
            Err(e @ MempoolError::ShutdownWithoutStream) => return Err(e),
            Err(MempoolError::ServerError(e)) => {
                tracing::warn!("Mempool stream request failed! Status: {e}.\nRetrying...");
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }

    Ok(())
}

/// Transaction status will be set to `Failed` if it's still unconfirmed when the chain reaches it's expiry height.
fn expire_transactions<W>(wallet: &mut W) -> Result<(), SyncError<W::Error>>
where
    W: SyncWallet + SyncTransactions,
{
    let last_known_chain_height = wallet
        .get_sync_state()
        .map_err(SyncError::WalletError)?
        .last_known_chain_height()
        .expect("wallet height must exist after scan ranges have been updated");
    let wallet_transactions = wallet
        .get_wallet_transactions_mut()
        .map_err(SyncError::WalletError)?;

    let expired_txids = wallet_transactions
        .values()
        .filter(|transaction| {
            transaction.status().is_pending()
                && last_known_chain_height >= transaction.transaction().expiry_height()
        })
        .map(super::wallet::WalletTransaction::txid)
        .collect::<Vec<_>>();
    set_transactions_failed(wallet_transactions, expired_txids);

    Ok(())
}

fn max_nullifier_map_size(performance_level: PerformanceLevel) -> Option<usize> {
    match performance_level {
        PerformanceLevel::Low => Some(0),
        PerformanceLevel::Medium => Some(125_000),
        PerformanceLevel::High => Some(2_000_000),
        PerformanceLevel::Maximum => None,
    }
}

#[cfg(test)]
mod test {
    mod process_scan_results_events {
        use std::collections::{BTreeMap, HashMap};

        use tokio::sync::mpsc;
        use zcash_primitives::block::BlockHash;
        use zcash_primitives::transaction::{TransactionData, TxId, TxVersion};
        use zcash_protocol::consensus::{BlockHeight, BranchId};
        use zcash_protocol::local_consensus::LocalNetwork;
        use zingo_status::confirmation_status::ConfirmationStatus;

        use crate::config::PerformanceLevel;
        use crate::events::SyncEvent;
        use crate::mocks::MockWalletBuilder;
        use crate::scan::ScanResults;
        use crate::sync::{ScanPriority, ScanRange, process_scan_results};
        use crate::wallet::{NullifierMap, SyncState, TreeBounds, WalletBlock, WalletTransaction};

        const LOCAL_NETWORK: LocalNetwork = LocalNetwork {
            overwinter: Some(BlockHeight::from_u32(1)),
            sapling: Some(BlockHeight::from_u32(3)),
            blossom: Some(BlockHeight::from_u32(3)),
            heartwood: Some(BlockHeight::from_u32(3)),
            canopy: Some(BlockHeight::from_u32(3)),
            nu5: Some(BlockHeight::from_u32(3)),
            nu6: Some(BlockHeight::from_u32(3)),
            nu6_1: Some(BlockHeight::from_u32(3)),
            nu6_2: Some(BlockHeight::from_u32(3)),
        };

        fn wallet_block(
            height: u32,
            sapling_outputs: u32,
            orchard_outputs: u32,
        ) -> (BlockHeight, WalletBlock) {
            let block_height = BlockHeight::from_u32(height);
            (
                block_height,
                WalletBlock {
                    block_height,
                    block_hash: BlockHash([0; 32]),
                    prev_hash: BlockHash([0; 32]),
                    time: 0,
                    txids: Vec::new(),
                    tree_bounds: TreeBounds {
                        sapling_initial_tree_size: 0,
                        sapling_final_tree_size: sapling_outputs,
                        orchard_initial_tree_size: 0,
                        orchard_final_tree_size: orchard_outputs,
                    },
                },
            )
        }

        fn empty_wallet_transaction(txid: TxId, status: ConfirmationStatus) -> WalletTransaction {
            let transaction = TransactionData::from_parts(
                TxVersion::V5,
                BranchId::Nu5,
                0,
                BlockHeight::from_u32(0),
                None,
                None,
                None,
                None,
            )
            .freeze()
            .expect("empty v5 transaction should always be valid");

            WalletTransaction {
                txid,
                status,
                transaction,
                datetime: 0,
                transparent_coins: Vec::new(),
                sapling_notes: Vec::new(),
                orchard_notes: Vec::new(),
                outgoing_sapling_notes: Vec::new(),
                outgoing_orchard_notes: Vec::new(),
            }
        }

        #[tokio::test]
        async fn range_scanned_and_tx_discovered_on_success() {
            let scan_range = ScanRange::from_parts(
                BlockHeight::from_u32(1)..BlockHeight::from_u32(10),
                ScanPriority::Historic,
            );
            // the processed range is mid-scan in the wallet's state, with a higher range already
            // scanned so no shard tree checkpoints are added (no fetcher in this test)
            let sync_state = SyncState {
                scan_ranges: vec![
                    ScanRange::from_parts(
                        BlockHeight::from_u32(1)..BlockHeight::from_u32(10),
                        ScanPriority::Scanning,
                    ),
                    ScanRange::from_parts(
                        BlockHeight::from_u32(10)..BlockHeight::from_u32(1001),
                        ScanPriority::Scanned,
                    ),
                ],
                ..Default::default()
            };
            let mut wallet = MockWalletBuilder::new()
                .sync_state(sync_state)
                .create_mock_wallet();

            let txid = TxId::from_bytes([1; 32]);
            let status = ConfirmationStatus::Confirmed(BlockHeight::from_u32(5));
            let scan_results = ScanResults {
                nullifiers: NullifierMap::new(),
                outpoints: BTreeMap::new(),
                scanned_blocks: (1..10).map(|height| wallet_block(height, 2, 1)).collect(),
                wallet_transactions: HashMap::from([(
                    txid,
                    empty_wallet_transaction(txid, status),
                )]),
                sapling_located_trees: Vec::new(),
                orchard_located_trees: Vec::new(),
            };

            let (fetch_request_sender, _fetch_request_receiver) = mpsc::unbounded_channel();
            let mut nullifier_map_limit_exceeded = false;
            let events = process_scan_results(
                &LOCAL_NETWORK,
                &mut wallet,
                fetch_request_sender,
                &HashMap::new(),
                scan_range.clone(),
                Ok(scan_results),
                BlockHeight::from_u32(1000),
                PerformanceLevel::High,
                &mut nullifier_map_limit_exceeded,
            )
            .await
            .unwrap();

            assert_eq!(events.len(), 2);
            match &events[0] {
                SyncEvent::RangeScanned {
                    range,
                    priority,
                    sapling_outputs,
                    orchard_outputs,
                } => {
                    assert_eq!(range, scan_range.block_range());
                    assert_eq!(*priority, ScanPriority::Historic);
                    // 9 blocks of 2 sapling and 1 orchard outputs each
                    assert_eq!(*sapling_outputs, 18);
                    assert_eq!(*orchard_outputs, 9);
                }
                event => panic!("expected RangeScanned, found {event:?}"),
            }
            match &events[1] {
                SyncEvent::TxDiscovered {
                    txid: event_txid,
                    status: event_status,
                } => {
                    assert_eq!(*event_txid, txid);
                    assert_eq!(*event_status, status);
                }
                event => panic!("expected TxDiscovered, found {event:?}"),
            }
        }
    }

    mod checked_height_validation {
        use zcash_protocol::consensus::BlockHeight;
        use zcash_protocol::local_consensus::LocalNetwork;
        const LOCAL_NETWORK: LocalNetwork = LocalNetwork {
            overwinter: Some(BlockHeight::from_u32(1)),
            sapling: Some(BlockHeight::from_u32(3)),
            blossom: Some(BlockHeight::from_u32(3)),
            heartwood: Some(BlockHeight::from_u32(3)),
            canopy: Some(BlockHeight::from_u32(3)),
            nu5: Some(BlockHeight::from_u32(3)),
            nu6: Some(BlockHeight::from_u32(3)),
            nu6_1: Some(BlockHeight::from_u32(3)),
            nu6_2: Some(BlockHeight::from_u32(3)),
        };
        use crate::{error::SyncError, mocks::MockWalletError, sync::checked_wallet_height};
        // It's possible an error from an implementor's get_sync_state could bubble up to checked_wallet_height
        // this test shows that such an error is raies wrapped in a WalletError and return as the Err variant
        #[tokio::test]
        async fn get_sync_state_error() {
            let builder = crate::mocks::MockWalletBuilder::new();
            let test_error = "get_sync_state_error";
            let mut test_wallet = builder
                .get_sync_state_patch(Box::new(|_| {
                    Err(MockWalletError::AnErrorVariant(test_error.to_string()))
                }))
                .create_mock_wallet();
            let res =
                checked_wallet_height(&mut test_wallet, BlockHeight::from_u32(1), &LOCAL_NETWORK);
            assert!(matches!(
                res,
                Err(SyncError::WalletError(
                    crate::mocks::MockWalletError::AnErrorVariant(ref s)
                )) if s == test_error
            ));
        }

        mod last_known_chain_height {
            use crate::{
                sync::{MAX_REORG_ALLOWANCE, ScanRange},
                wallet::SyncState,
            };
            const DEFAULT_START_HEIGHT: BlockHeight = BlockHeight::from_u32(1);
            const _DEFAULT_LAST_KNOWN_HEIGHT: BlockHeight = BlockHeight::from_u32(102);
            const DEFAULT_CHAIN_HEIGHT: BlockHeight = BlockHeight::from_u32(110);

            use super::*;
            #[tokio::test]
            async fn above_allowance() {
                const LAST_KNOWN_HEIGHT: BlockHeight = BlockHeight::from_u32(211);
                let lkch = vec![ScanRange::from_parts(
                    DEFAULT_START_HEIGHT..LAST_KNOWN_HEIGHT,
                    crate::sync::ScanPriority::Scanned,
                )];
                let state = SyncState {
                    scan_ranges: lkch,
                    ..Default::default()
                };
                let builder = crate::mocks::MockWalletBuilder::new();
                let mut test_wallet = builder.sync_state(state).create_mock_wallet();
                let res =
                    checked_wallet_height(&mut test_wallet, DEFAULT_CHAIN_HEIGHT, &LOCAL_NETWORK);
                if let Err(e) = res {
                    assert_eq!(
                        e.to_string(),
                        format!(
                            "wallet height {} is more than {} blocks ahead of best chain height {}",
                            LAST_KNOWN_HEIGHT - 1,
                            MAX_REORG_ALLOWANCE,
                            DEFAULT_CHAIN_HEIGHT
                        )
                    );
                } else {
                    panic!()
                }
            }
            #[tokio::test]
            async fn above_chain_height_below_allowance() {
                // The hain_height is received from the proxy
                // truncate uses the wallet scan start height
                // as a
                let lkch = vec![ScanRange::from_parts(
                    BlockHeight::from_u32(6)..BlockHeight::from_u32(10),
                    crate::sync::ScanPriority::Scanned,
                )];
                let state = SyncState {
                    scan_ranges: lkch,
                    ..Default::default()
                };
                let builder = crate::mocks::MockWalletBuilder::new();
                let mut test_wallet = builder.sync_state(state).create_mock_wallet();
                let chain_height = BlockHeight::from_u32(4);
                // This will trigger a call to truncate_wallet_data with
                // chain_height and start_height inferred from the wallet.
                // chain must be greater than by this time which hits the Greater cmp
                // match
                let res = checked_wallet_height(&mut test_wallet, chain_height, &LOCAL_NETWORK);
                assert_eq!(res.unwrap(), BlockHeight::from_u32(4));
            }
            #[ignore = "in progress"]
            #[tokio::test]
            async fn equal_or_below_chain_height_and_above_sapling() {
                let lkch = vec![ScanRange::from_parts(
                    BlockHeight::from_u32(1)..BlockHeight::from_u32(10),
                    crate::sync::ScanPriority::Scanned,
                )];
                let state = SyncState {
                    scan_ranges: lkch,
                    ..Default::default()
                };
                let builder = crate::mocks::MockWalletBuilder::new();
                let mut _test_wallet = builder.sync_state(state).create_mock_wallet();
            }
            #[ignore = "in progress"]
            #[tokio::test]
            async fn equal_or_below_chain_height_and_below_sapling() {
                // This case requires that the wallet have a scan_start_below sapling
                // which is an unexpected state.
                let lkch = vec![ScanRange::from_parts(
                    BlockHeight::from_u32(1)..BlockHeight::from_u32(10),
                    crate::sync::ScanPriority::Scanned,
                )];
                let state = SyncState {
                    scan_ranges: lkch,
                    ..Default::default()
                };
                let builder = crate::mocks::MockWalletBuilder::new();
                let mut _test_wallet = builder.sync_state(state).create_mock_wallet();
            }
            #[ignore = "in progress"]
            #[tokio::test]
            async fn below_sapling() {
                let lkch = vec![ScanRange::from_parts(
                    BlockHeight::from_u32(1)..BlockHeight::from_u32(10),
                    crate::sync::ScanPriority::Scanned,
                )];
                let state = SyncState {
                    scan_ranges: lkch,
                    ..Default::default()
                };
                let builder = crate::mocks::MockWalletBuilder::new();
                let mut _test_wallet = builder.sync_state(state).create_mock_wallet();
            }
        }
        mod no_last_known_chain_height {
            use super::*;
            // If there are know scan_ranges in the SyncState
            #[tokio::test]
            async fn get_bday_error() {
                let test_error = "get_bday_error";
                let builder = crate::mocks::MockWalletBuilder::new();
                let mut test_wallet = builder
                    .get_birthday_patch(Box::new(|_| {
                        Err(crate::mocks::MockWalletError::AnErrorVariant(
                            test_error.to_string(),
                        ))
                    }))
                    .create_mock_wallet();
                let res = checked_wallet_height(
                    &mut test_wallet,
                    BlockHeight::from_u32(1),
                    &LOCAL_NETWORK,
                );
                assert!(matches!(
                    res,
                    Err(SyncError::WalletError(
                        crate::mocks::MockWalletError::AnErrorVariant(ref s)
                    )) if s == test_error
                ));
            }
            #[ignore = "in progress"]
            #[tokio::test]
            async fn raw_bday_above_chain_height() {
                let builder = crate::mocks::MockWalletBuilder::new();
                let mut test_wallet = builder
                    .birthday(BlockHeight::from_u32(15))
                    .create_mock_wallet();
                let res = checked_wallet_height(
                    &mut test_wallet,
                    BlockHeight::from_u32(1),
                    &LOCAL_NETWORK,
                );
                if let Err(e) = res {
                    assert_eq!(
                        e.to_string(),
                        format!(
                            "wallet height is more than {} blocks ahead of best chain height",
                            15 - 1
                        )
                    );
                } else {
                    panic!()
                }
            }
            mod sapling_height {
                use super::*;
                #[tokio::test]
                async fn raw_bday_above() {
                    let builder = crate::mocks::MockWalletBuilder::new();
                    let mut test_wallet = builder
                        .birthday(BlockHeight::from_u32(4))
                        .create_mock_wallet();
                    let res = checked_wallet_height(
                        &mut test_wallet,
                        BlockHeight::from_u32(5),
                        &LOCAL_NETWORK,
                    );
                    assert_eq!(res.unwrap(), BlockHeight::from_u32(4 - 1));
                }
                #[tokio::test]
                async fn raw_bday_equal() {
                    let builder = crate::mocks::MockWalletBuilder::new();
                    let mut test_wallet = builder
                        .birthday(BlockHeight::from_u32(3))
                        .create_mock_wallet();
                    let res = checked_wallet_height(
                        &mut test_wallet,
                        BlockHeight::from_u32(5),
                        &LOCAL_NETWORK,
                    );
                    assert_eq!(res.unwrap(), BlockHeight::from_u32(3 - 1));
                }
                #[tokio::test]
                async fn raw_bday_below() {
                    let builder = crate::mocks::MockWalletBuilder::new();
                    let mut test_wallet = builder
                        .birthday(BlockHeight::from_u32(1))
                        .create_mock_wallet();
                    let res = checked_wallet_height(
                        &mut test_wallet,
                        BlockHeight::from_u32(5),
                        &LOCAL_NETWORK,
                    );
                    assert!(matches!(res, Err(SyncError::BirthdayBelowSapling(1, 3))));
                }
            }
        }
    }
}
