use std::{
    collections::{BTreeSet, HashMap},
    sync::{
        Arc,
        atomic::{self, AtomicBool},
    },
    time::Duration,
};

use tokio::{
    sync::{Notify, mpsc},
    task::JoinSet,
};

use zcash_keys::keys::UnifiedFullViewingKey;
use zcash_primitives::transaction::TxId;
use zcash_protocol::consensus::{self, BlockHeight};
use zingo_netutils::lightwallet_protocol::CompactBlock;
use zip32::AccountId;

use crate::{
    client::{self, FetchRequest},
    config::PerformanceLevel,
    error::{ScanError, ServerError, SyncError},
    events::{SyncEmitter, SyncEvent},
    keys::transparent::TransparentAddressId,
    scan::get_compact_block_height,
    sync::{self, ScanPriority, ScanRange},
    wallet::{
        ScanTarget, WalletBlock,
        traits::{SyncBlocks, SyncNullifiers, SyncWallet},
    },
};

use super::{ScanResults, scan};

const MAX_WORKER_POOLSIZE: usize = 2;
const MAX_BATCH_NULLIFIERS: usize = 2usize.pow(14);

const STREAM_OPEN_TIMEOUT: Duration = Duration::from_secs(10);
const STREAM_MSG_TIMEOUT: Duration = Duration::from_secs(15);
const SCAN_TASK_TIMEOUT: Duration = Duration::from_secs(120);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

pub(crate) enum ScannerState {
    Verification,
    Scan,
    Shutdown,
}

impl ScannerState {
    fn verified(&mut self) {
        *self = ScannerState::Scan;
    }

    fn shutdown(&mut self) {
        *self = ScannerState::Shutdown;
    }
}

pub(crate) struct Scanner<P> {
    pub(crate) state: ScannerState,
    /// Signalled whenever [`Self::update`] may make progress: a batch is ready, the batcher or a
    /// worker went idle, or the wallet's scan ranges changed.
    pub(crate) wake: Arc<Notify>,
    batcher: Option<Batcher<P>>,
    workers: Vec<ScanWorker<P>>,
    tasks: JoinSet<Result<(), ServerError>>,
    unique_id: usize,
    scan_results_sender: mpsc::UnboundedSender<(ScanRange, Result<ScanResults, ScanError>)>,
    fetch_request_sender: mpsc::UnboundedSender<FetchRequest>,
    consensus_parameters: P,
    ufvks: HashMap<AccountId, UnifiedFullViewingKey>,
    events: SyncEmitter,
    last_scan_plan: Vec<ScanRange>,
}

impl<P> Scanner<P>
where
    P: consensus::Parameters + Sync + Send + 'static,
{
    pub(crate) fn new(
        consensus_parameters: P,
        scan_results_sender: mpsc::UnboundedSender<(ScanRange, Result<ScanResults, ScanError>)>,
        fetch_request_sender: mpsc::UnboundedSender<FetchRequest>,
        ufvks: HashMap<AccountId, UnifiedFullViewingKey>,
        events: SyncEmitter,
    ) -> Self {
        let workers: Vec<ScanWorker<P>> = Vec::with_capacity(MAX_WORKER_POOLSIZE);

        Self {
            state: ScannerState::Verification,
            wake: Arc::new(Notify::new()),
            batcher: None,
            workers,
            tasks: JoinSet::new(),
            unique_id: 0,
            scan_results_sender,
            fetch_request_sender,
            consensus_parameters,
            ufvks,
            events,
            last_scan_plan: Vec::new(),
        }
    }

    pub(crate) fn set_scan_plan(&mut self, ranges: Vec<ScanRange>) {
        self.last_scan_plan = ranges;
    }

    pub(crate) fn emit_scan_plan_if_changed<W>(&mut self, wallet: &W) -> Result<(), W::Error>
    where
        W: SyncWallet,
    {
        let ranges = wallet.get_sync_state()?.scan_ranges().to_vec();
        if ranges != self.last_scan_plan {
            self.last_scan_plan = ranges.clone();
            self.events.emit(SyncEvent::ScanPlanUpdated { ranges });
        }
        Ok(())
    }

    pub(crate) fn launch(&mut self, performance_level: PerformanceLevel) {
        let max_batch_outputs = match performance_level {
            PerformanceLevel::Low => 2usize.pow(11),
            PerformanceLevel::Medium => 2usize.pow(13),
            PerformanceLevel::High => 2usize.pow(13),
            PerformanceLevel::Maximum => 2usize.pow(15),
        };

        self.spawn_batcher(max_batch_outputs);
        self.spawn_workers(max_batch_outputs);
        self.wake.notify_one();
    }

    /// Resolves when a batcher or worker task exits, surfacing its error.
    ///
    /// Before [`Self::shutdown`] only a failed batcher exits, so this is the sync loop's error
    /// signal for the scan tasks.
    pub(crate) async fn join_next(&mut self) -> Option<Result<(), ServerError>> {
        self.tasks
            .join_next()
            .await
            .map(|joined| joined.expect("scan task panicked"))
    }

    /// Closes the batcher's and workers' task channels and joins them as they exit.
    pub(crate) async fn shutdown(mut self) -> Result<(), ServerError> {
        tracing::debug!("Shutting down scanner");
        self.batcher = None;
        self.workers.clear();
        let joined = tokio::time::timeout(SHUTDOWN_TIMEOUT, async {
            while let Some(joined) = self.tasks.join_next().await {
                joined.expect("scan task panicked")?;
            }
            Ok::<(), ServerError>(())
        })
        .await;
        match joined {
            Ok(joined) => joined,
            Err(_) => {
                self.tasks.shutdown().await;
                Err(tonic::Status::deadline_exceeded("scanner shutdown timeout").into())
            }
        }
    }

    /// Spawns the batcher.
    ///
    /// When the batcher is running it will wait for a scan task.
    pub(crate) fn spawn_batcher(&mut self, max_batch_outputs: usize) {
        tracing::debug!("Spawning batcher");
        let mut batcher = Batcher::new(
            self.consensus_parameters.clone(),
            self.fetch_request_sender.clone(),
        );
        batcher.run(max_batch_outputs, &mut self.tasks, self.wake.clone());
        self.batcher = Some(batcher);
    }

    /// Spawns a worker.
    ///
    /// When the worker is running it will wait for a scan task.
    pub(crate) fn spawn_worker(&mut self, max_batch_outputs: usize) {
        tracing::debug!("Spawning worker {}", self.unique_id);
        let mut worker = ScanWorker::new(
            self.unique_id,
            self.consensus_parameters.clone(),
            self.scan_results_sender.clone(),
            self.fetch_request_sender.clone(),
            self.ufvks.clone(),
            self.events.clone(),
        );
        worker.run(max_batch_outputs, &mut self.tasks, self.wake.clone());
        self.workers.push(worker);
        self.unique_id += 1;
    }

    /// Spawns the initial pool of workers.
    ///
    /// Poolsize is set by [`self::MAX_WORKER_POOLSIZE`].
    pub(crate) fn spawn_workers(&mut self, max_batch_outputs: usize) {
        for _ in 0..MAX_WORKER_POOLSIZE {
            self.spawn_worker(max_batch_outputs);
        }
    }

    /// Updates the scanner.
    ///
    /// Creates a new scan task and sends to batcher if it's idle.
    /// The batcher will stream compact blocks into the scan task, splitting the scan task when the maximum number of
    /// outputs is reached. When a scan task is ready it is stored in the batcher ready to be taken by an idle scan
    /// worker for scanning.
    /// When verification is still in progress, only scan tasks with `Verify` scan priority are created.
    /// When all ranges are scanned the scanner enters [`ScannerState::Shutdown`] and the sync loop
    /// calls [`Self::shutdown`].
    pub(crate) fn update<W>(
        &mut self,
        wallet: &mut W,
        nullifier_map_limit_exceeded: bool,
    ) -> Result<(), SyncError<W::Error>>
    where
        W: SyncWallet + SyncBlocks + SyncNullifiers,
    {
        match self.state {
            ScannerState::Verification => {
                self.update_workers();

                let sync_state = wallet.get_sync_state().map_err(SyncError::WalletError)?;
                if !sync_state
                    .scan_ranges()
                    .iter()
                    .any(|scan_range| scan_range.priority() == ScanPriority::Verify)
                {
                    if sync_state
                        .scan_ranges()
                        .iter()
                        .any(|scan_range| scan_range.priority() == ScanPriority::Scanning)
                    {
                        // the last scan ranges with `Verify` priority are currently being scanned.
                        return Ok(());
                    }
                    // verification complete, so scanning starts without waiting for another wake
                    self.state.verified();
                    self.update_batcher(wallet, nullifier_map_limit_exceeded)
                        .map_err(SyncError::WalletError)?;
                    return Ok(());
                }

                // scan ranges with `Verify` priority
                self.update_batcher(wallet, nullifier_map_limit_exceeded)
                    .map_err(SyncError::WalletError)?;
            }
            ScannerState::Scan => {
                self.update_workers();
                self.update_batcher(wallet, nullifier_map_limit_exceeded)
                    .map_err(SyncError::WalletError)?;
            }
            ScannerState::Shutdown => (),
        }

        Ok(())
    }

    /// Hands ready batches to idle workers until either runs out.
    fn update_workers(&mut self) {
        let batcher = self.batcher.as_mut().expect("batcher should be running");
        while let Some(worker_index) = self.workers.iter().position(|worker| !worker.is_scanning())
        {
            batcher.update_batch_store();
            let Some(batch) = batcher.batch.take() else {
                break;
            };
            // nullifier refetch batches announce nothing, mirroring the commit side where
            // refetch ranges emit no `RangeScanned`
            if batch.scan_range.priority() != ScanPriority::ScannedWithoutMapping {
                let (sapling_outputs, orchard_outputs, ironwood_outputs) = batch.output_counts();
                self.events.emit(SyncEvent::BatchScanStarted {
                    range: batch.scan_range.block_range().clone(),
                    priority: batch.scan_range.priority(),
                    sapling_outputs,
                    orchard_outputs,
                    ironwood_outputs,
                });
            }
            self.workers[worker_index].add_scan_task(batch);
        }
    }

    fn update_batcher<W>(
        &mut self,
        wallet: &mut W,
        nullifier_map_limit_exceeded: bool,
    ) -> Result<(), W::Error>
    where
        W: SyncWallet + SyncBlocks + SyncNullifiers,
    {
        let batcher = self.batcher.as_ref().expect("batcher should be running");
        if !batcher.is_batching() {
            if let Some(scan_task) = sync::state::create_scan_task(
                &self.consensus_parameters,
                wallet,
                nullifier_map_limit_exceeded,
            )? {
                batcher.add_scan_task(scan_task);
            } else if wallet.get_sync_state()?.scan_complete() {
                self.state.shutdown();
            }
        }

        Ok(())
    }
}

struct Batcher<P> {
    is_batching: Arc<AtomicBool>,
    batch: Option<ScanTask>,
    consensus_parameters: P,
    scan_task_sender: Option<mpsc::Sender<ScanTask>>,
    batch_receiver: Option<mpsc::Receiver<ScanTask>>,
    fetch_request_sender: mpsc::UnboundedSender<FetchRequest>,
}

impl<P> Batcher<P>
where
    P: consensus::Parameters + Sync + Send + 'static,
{
    fn new(
        consensus_parameters: P,
        fetch_request_sender: mpsc::UnboundedSender<FetchRequest>,
    ) -> Self {
        Self {
            is_batching: Arc::new(AtomicBool::new(false)),
            batch: None,
            consensus_parameters,
            scan_task_sender: None,
            batch_receiver: None,
            fetch_request_sender,
        }
    }

    /// Runs the batcher in a new tokio task.
    ///
    /// Waits for a scan task and then fetches compact blocks to form fixed output batches. The scan task is split if
    /// needed and the compact blocks are added to each scan task and sent to the scan workers for scanning.
    /// `wake` is signalled whenever a batch is ready and when the batcher goes idle.
    fn run(
        &mut self,
        max_batch_outputs: usize,
        tasks: &mut JoinSet<Result<(), ServerError>>,
        wake: Arc<Notify>,
    ) {
        let (scan_task_sender, mut scan_task_receiver) = mpsc::channel::<ScanTask>(1);
        let (batch_sender, batch_receiver) = mpsc::channel::<ScanTask>(1);

        let is_batching = self.is_batching.clone();
        let fetch_request_sender = self.fetch_request_sender.clone();
        let consensus_parameters = self.consensus_parameters.clone();

        tasks.spawn(async move {
            // save seam blocks between scan tasks for linear scanning continuity checks
            // during non-linear scanning the wallet blocks from the scanned ranges will already be saved in the wallet
            let mut previous_task_first_block: Option<WalletBlock> = None;
            let mut previous_task_last_block: Option<WalletBlock> = None;

            while let Some(mut scan_task) = scan_task_receiver.recv().await {
                let fetch_nullifiers_only =
                    scan_task.scan_range.priority() == ScanPriority::ScannedWithoutMapping;

                let mut retry_height = scan_task.scan_range.block_range().start;
                let mut sapling_output_count = 0;
                let mut orchard_output_count = 0;
                let mut ironwood_output_count = 0;
                let mut sapling_nullifier_count = 0;
                let mut orchard_nullifier_count = 0;
                let mut ironwood_nullifier_count = 0;
                let mut first_batch = true;

                let mut block_stream = {
                    let range = scan_task.scan_range.block_range().clone();
                    let frs = fetch_request_sender.clone();

                    let open_fut = async move {
                        if fetch_nullifiers_only {
                            client::get_nullifier_range(frs, range).await
                        } else {
                            client::get_compact_block_range(frs, range).await
                        }
                    };

                    match tokio::time::timeout(STREAM_OPEN_TIMEOUT, open_fut).await {
                        Ok(res) => res?,
                        Err(_) => {
                            return Err(
                                tonic::Status::deadline_exceeded("open stream timeout").into()
                            );
                        }
                    }
                };

                loop {
                    let msg_res: Result<Option<CompactBlock>, tonic::Status> =
                        match tokio::time::timeout(STREAM_MSG_TIMEOUT, block_stream.message()).await
                        {
                            Ok(res) => res,
                            Err(_) => {
                                Err(tonic::Status::deadline_exceeded("stream message timeout"))
                            }
                        };

                    let maybe_block = match msg_res {
                        Ok(b) => b,
                        Err(e)
                            if e.code() == tonic::Code::DeadlineExceeded
                                || e.message().contains("Unexpected EOF decoding stream.") =>
                        {
                            tokio::time::sleep(Duration::from_secs(3)).await;

                            let retry_range = retry_height..scan_task.scan_range.block_range().end;

                            let reopen_fut = {
                                let frs = fetch_request_sender.clone();

                                async move {
                                    if fetch_nullifiers_only {
                                        client::get_nullifier_range(frs, retry_range).await
                                    } else {
                                        client::get_compact_block_range(frs, retry_range).await
                                    }
                                }
                            };

                            block_stream =
                                match tokio::time::timeout(STREAM_OPEN_TIMEOUT, reopen_fut).await {
                                    Ok(res) => res?,
                                    Err(_) => {
                                        return Err(tonic::Status::deadline_exceeded(
                                            "open stream timeout (retry)",
                                        )
                                        .into());
                                    }
                                };

                            let first_msg_res: Result<Option<CompactBlock>, tonic::Status> =
                                match tokio::time::timeout(
                                    STREAM_MSG_TIMEOUT,
                                    block_stream.message(),
                                )
                                .await
                                {
                                    Ok(res) => res,
                                    Err(_) => Err(tonic::Status::deadline_exceeded(
                                        "stream message timeout after retry",
                                    )),
                                };

                            match first_msg_res {
                                Ok(b) => b,
                                Err(e) => return Err(e.into()),
                            }
                        }
                        Err(e) => {
                            return Err(e.into());
                        }
                    };

                    let Some(compact_block) = maybe_block else {
                        break;
                    };

                    if fetch_nullifiers_only {
                        sapling_nullifier_count += compact_block
                            .vtx
                            .iter()
                            .fold(0, |acc, transaction| acc + transaction.spends.len());
                        orchard_nullifier_count += compact_block
                            .vtx
                            .iter()
                            .fold(0, |acc, transaction| acc + transaction.actions.len());
                        ironwood_nullifier_count +=
                            compact_block.vtx.iter().fold(0, |acc, transaction| {
                                acc + transaction.ironwood_actions.len()
                            });
                    } else {
                        if let Some(block) = previous_task_last_block.as_ref()
                            && scan_task.start_seam_block.is_none()
                            && scan_task.scan_range.block_range().start == block.block_height() + 1
                        {
                            scan_task.start_seam_block = previous_task_last_block.clone();
                        }
                        if let Some(block) = previous_task_first_block.as_ref()
                            && scan_task.end_seam_block.is_none()
                            && scan_task.scan_range.block_range().end == block.block_height()
                        {
                            scan_task.end_seam_block = previous_task_first_block.clone();
                        }
                        if first_batch {
                            previous_task_first_block = Some(
                                WalletBlock::from_compact_block(
                                    &consensus_parameters,
                                    fetch_request_sender.clone(),
                                    &compact_block,
                                )
                                .await?,
                            );
                            first_batch = false;
                        }
                        if get_compact_block_height(&compact_block)
                            == scan_task.scan_range.block_range().end - 1
                        {
                            previous_task_last_block = Some(
                                WalletBlock::from_compact_block(
                                    &consensus_parameters,
                                    fetch_request_sender.clone(),
                                    &compact_block,
                                )
                                .await?,
                            );
                        }

                        sapling_output_count += compact_block
                            .vtx
                            .iter()
                            .fold(0, |acc, transaction| acc + transaction.outputs.len());
                        orchard_output_count += compact_block
                            .vtx
                            .iter()
                            .fold(0, |acc, transaction| acc + transaction.actions.len());
                        ironwood_output_count +=
                            compact_block.vtx.iter().fold(0, |acc, transaction| {
                                acc + transaction.ironwood_actions.len()
                            });
                    }

                    if sapling_output_count + orchard_output_count + ironwood_output_count
                        > max_batch_outputs
                        || sapling_nullifier_count
                            + orchard_nullifier_count
                            + ironwood_nullifier_count
                            > MAX_BATCH_NULLIFIERS
                    {
                        let (full_batch, new_batch) = scan_task
                            .clone()
                            .split(
                                &consensus_parameters,
                                fetch_request_sender.clone(),
                                get_compact_block_height(&compact_block),
                            )
                            .await?;

                        let _ignore_error = batch_sender.send(full_batch).await;
                        wake.notify_one();

                        scan_task = new_batch;
                        sapling_output_count = 0;
                        orchard_output_count = 0;
                        ironwood_output_count = 0;
                        sapling_nullifier_count = 0;
                        orchard_nullifier_count = 0;
                        ironwood_nullifier_count = 0;
                    }

                    retry_height = get_compact_block_height(&compact_block) + 1;
                    scan_task.compact_blocks.push(compact_block);
                }

                let _ignore_error = batch_sender.send(scan_task).await;

                is_batching.store(false, atomic::Ordering::Release);
                wake.notify_one();
            }

            is_batching.store(false, atomic::Ordering::Release);

            Ok(())
        });

        self.scan_task_sender = Some(scan_task_sender);
        self.batch_receiver = Some(batch_receiver);
    }

    fn is_batching(&self) -> bool {
        self.is_batching.load(atomic::Ordering::Acquire)
    }

    fn add_scan_task(&self, scan_task: ScanTask) {
        tracing::trace!("Adding scan task to batcher:\n{:#?}", &scan_task);
        self.scan_task_sender
            .clone()
            .expect("batcher should be running")
            .try_send(scan_task)
            .expect("batcher should never be sent multiple tasks at one time");
        self.is_batching.store(true, atomic::Ordering::Release);
    }

    fn update_batch_store(&mut self) {
        let batch_receiver = self
            .batch_receiver
            .as_mut()
            .expect("batcher should be running");
        if self.batch.is_none() && !batch_receiver.is_empty() {
            self.batch = Some(
                batch_receiver
                    .try_recv()
                    .expect("channel should be non-empty!"),
            );
        }
    }
}

pub(crate) struct ScanWorker<P> {
    id: usize,
    is_scanning: Arc<AtomicBool>,
    consensus_parameters: P,
    scan_task_sender: Option<mpsc::Sender<ScanTask>>,
    scan_results_sender: mpsc::UnboundedSender<(ScanRange, Result<ScanResults, ScanError>)>,
    fetch_request_sender: mpsc::UnboundedSender<FetchRequest>,
    ufvks: HashMap<AccountId, UnifiedFullViewingKey>,
    events: SyncEmitter,
}

impl<P> ScanWorker<P>
where
    P: consensus::Parameters + Sync + Send + 'static,
{
    fn new(
        id: usize,
        consensus_parameters: P,
        scan_results_sender: mpsc::UnboundedSender<(ScanRange, Result<ScanResults, ScanError>)>,
        fetch_request_sender: mpsc::UnboundedSender<FetchRequest>,
        ufvks: HashMap<AccountId, UnifiedFullViewingKey>,
        events: SyncEmitter,
    ) -> Self {
        Self {
            id,
            is_scanning: Arc::new(AtomicBool::new(false)),
            consensus_parameters,
            scan_task_sender: None,
            scan_results_sender,
            fetch_request_sender,
            ufvks,
            events,
        }
    }

    /// Runs the worker in a new tokio task.
    ///
    /// Waits for a scan task and then calls [`crate::scan::scan`] on the given range.
    /// `wake` is signalled when the worker goes idle.
    fn run(
        &mut self,
        max_batch_outputs: usize,
        tasks: &mut JoinSet<Result<(), ServerError>>,
        wake: Arc<Notify>,
    ) {
        let (scan_task_sender, mut scan_task_receiver) = mpsc::channel::<ScanTask>(1);

        let is_scanning = self.is_scanning.clone();
        let scan_results_sender = self.scan_results_sender.clone();
        let fetch_request_sender = self.fetch_request_sender.clone();
        let consensus_parameters = self.consensus_parameters.clone();
        let ufvks = self.ufvks.clone();
        let events = self.events.clone();

        tasks.spawn(async move {
            while let Some(scan_task) = scan_task_receiver.recv().await {
                let scan_range = scan_task.scan_range.clone();

                let scan_fut = scan(
                    fetch_request_sender.clone(),
                    &consensus_parameters,
                    &ufvks,
                    scan_task,
                    max_batch_outputs,
                );

                let scan_results = match tokio::time::timeout(SCAN_TASK_TIMEOUT, scan_fut).await {
                    Ok(res) => res,
                    Err(_) => {
                        // Best-effort: maps timeout into existing error types.
                        Err(ServerError::from(tonic::Status::deadline_exceeded(
                            "scan task timeout",
                        ))
                        .into())
                    }
                };

                // scanning is done and the result now queues for the serialized commit stage.
                // nullifier refetch batches announce nothing, mirroring the commit side.
                if scan_range.priority() != ScanPriority::ScannedWithoutMapping {
                    events.emit(SyncEvent::BatchScanCompleted {
                        range: scan_range.block_range().clone(),
                    });
                }
                let _ignore_error = scan_results_sender.send((scan_range, scan_results));

                is_scanning.store(false, atomic::Ordering::Release);
                wake.notify_one();
            }

            is_scanning.store(false, atomic::Ordering::Release);

            Ok(())
        });

        self.scan_task_sender = Some(scan_task_sender);
    }

    fn is_scanning(&self) -> bool {
        self.is_scanning.load(atomic::Ordering::Acquire)
    }

    fn add_scan_task(&self, scan_task: ScanTask) {
        tracing::trace!("Adding scan task to worker {}:\n{:#?}", self.id, &scan_task);
        self.scan_task_sender
            .clone()
            .expect("worker should be running")
            .try_send(scan_task)
            .expect("worker should never be sent multiple tasks at one time");
        self.is_scanning.store(true, atomic::Ordering::Release);
    }
}

#[derive(Debug, Clone)]
pub(crate) struct ScanTask {
    pub(crate) compact_blocks: Vec<CompactBlock>,
    pub(crate) scan_range: ScanRange,
    pub(crate) start_seam_block: Option<WalletBlock>,
    pub(crate) end_seam_block: Option<WalletBlock>,
    pub(crate) scan_targets: BTreeSet<ScanTarget>,
    pub(crate) transparent_addresses: HashMap<String, TransparentAddressId>,
}

impl ScanTask {
    /// Returns the (sapling, orchard, ironwood) note commitment counts across the task's compact
    /// blocks.
    ///
    /// Matches the quantities a consumer later receives on the pairing
    /// [`crate::events::SyncEvent::RangeScanned`], which sums tree-size deltas over the same
    /// blocks.
    fn output_counts(&self) -> (u32, u32, u32) {
        self.compact_blocks
            .iter()
            .fold((0u32, 0u32, 0u32), |acc, block| {
                block
                    .vtx
                    .iter()
                    .fold(acc, |(sapling, orchard, ironwood), transaction| {
                        (
                            sapling + transaction.outputs.len() as u32,
                            orchard + transaction.actions.len() as u32,
                            ironwood + transaction.ironwood_actions.len() as u32,
                        )
                    })
            })
    }

    pub(crate) fn from_parts(
        scan_range: ScanRange,
        start_seam_block: Option<WalletBlock>,
        end_seam_block: Option<WalletBlock>,
        scan_targets: BTreeSet<ScanTarget>,
        transparent_addresses: HashMap<String, TransparentAddressId>,
    ) -> Self {
        Self {
            compact_blocks: Vec::new(),
            scan_range,
            start_seam_block,
            end_seam_block,
            scan_targets,
            transparent_addresses,
        }
    }

    /// Splits a scan task into two at `block_height`.
    ///
    /// Panics if `block_height` is not contained in the scan task's block range.
    async fn split(
        self,
        consensus_parameters: &impl consensus::Parameters,
        fetch_request_sender: mpsc::UnboundedSender<FetchRequest>,
        block_height: BlockHeight,
    ) -> Result<(Self, Self), ServerError> {
        if block_height < self.scan_range.block_range().start
            && block_height > self.scan_range.block_range().end - 1
        {
            panic!("block height should be within scan tasks block range!");
        }

        let mut lower_compact_blocks = self.compact_blocks;
        let upper_compact_blocks = if let Some(index) = lower_compact_blocks
            .iter()
            .position(|block| get_compact_block_height(block) == block_height)
        {
            lower_compact_blocks.split_off(index)
        } else {
            Vec::new()
        };

        let mut lower_task_scan_targets = self.scan_targets;
        let upper_task_scan_targets = lower_task_scan_targets.split_off(&ScanTarget {
            block_height,
            txid: TxId::from_bytes([0; 32]),
            narrow_scan_area: false,
        });

        let lower_task_last_block = if let Some(block) = lower_compact_blocks.last() {
            Some(
                WalletBlock::from_compact_block(
                    consensus_parameters,
                    fetch_request_sender.clone(),
                    block,
                )
                .await?,
            )
        } else {
            None
        };
        let upper_task_first_block = if let Some(block) = upper_compact_blocks.first() {
            Some(
                WalletBlock::from_compact_block(
                    consensus_parameters,
                    fetch_request_sender.clone(),
                    block,
                )
                .await?,
            )
        } else {
            None
        };

        Ok((
            ScanTask {
                compact_blocks: lower_compact_blocks,
                scan_range: self
                    .scan_range
                    .truncate_end(block_height)
                    .expect("block height should be within block range"),
                start_seam_block: self.start_seam_block,
                end_seam_block: upper_task_first_block,
                scan_targets: lower_task_scan_targets,
                transparent_addresses: self.transparent_addresses.clone(),
            },
            ScanTask {
                compact_blocks: upper_compact_blocks,
                scan_range: self
                    .scan_range
                    .truncate_start(block_height)
                    .expect("block height should be within block range"),
                start_seam_block: lower_task_last_block,
                end_seam_block: self.end_seam_block,
                scan_targets: upper_task_scan_targets,
                transparent_addresses: self.transparent_addresses,
            },
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};

    use zcash_protocol::consensus::BlockHeight;
    use zingo_netutils::lightwallet_protocol::{
        CompactBlock, CompactOrchardAction, CompactSaplingOutput, CompactTx,
    };

    use crate::sync::{ScanPriority, ScanRange};

    use super::ScanTask;

    fn compact_block(
        sapling_outputs: usize,
        orchard_actions: usize,
        ironwood_actions: usize,
    ) -> CompactBlock {
        CompactBlock {
            vtx: vec![CompactTx {
                outputs: vec![CompactSaplingOutput::default(); sapling_outputs],
                actions: vec![CompactOrchardAction::default(); orchard_actions],
                ironwood_actions: vec![CompactOrchardAction::default(); ironwood_actions],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn output_counts_sum_over_compact_blocks() {
        let mut scan_task = ScanTask::from_parts(
            ScanRange::from_parts(
                BlockHeight::from_u32(1)..BlockHeight::from_u32(4),
                ScanPriority::Historic,
            ),
            None,
            None,
            BTreeSet::new(),
            HashMap::new(),
        );
        scan_task.compact_blocks = vec![
            compact_block(2, 1, 4),
            compact_block(0, 0, 0),
            compact_block(3, 5, 3),
        ];

        assert_eq!(scan_task.output_counts(), (5, 6, 7));
    }
}
