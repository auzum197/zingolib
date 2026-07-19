//! Watch the sync event stream for a view-only wallet.
//!
//! Creates a throwaway wallet from a UFVK, syncs it, and prints transactions the moment the
//! engine commits them. The live status block shows overall progress with an ETA, plus one
//! indented line per scan range with an estimated batch bar and spinner. When a batch commits,
//! its line is marked DONE in place and lingers next to any still-scanning sibling until the
//! next batch starts, when it is flushed into the scrollback log. Progress is computed from the
//! event stream (the consumer-side recipe from SYNC_UX_SPEC.md §7.3). Each batch line follows
//! the engine's lifecycle: a scanning bar, then `WAITING FOR OTHER TASKS` while it is blocked
//! behind the serialized commit stage, then a committing bar, each estimated from the engine's
//! measured per-phase timing. A finished batch prints its wait and per-phase times. Nothing is
//! written to disk.
//!
//! ```text
//! cargo run --release -p zingolib --example sync_events -- <UFVK> <BIRTHDAY> \
//!     [--server <URI>] [--chain mainnet|testnet]
//! ```
//!
//! Ctrl-C stops the engine after the current batch and still prints the sync result.

use std::collections::VecDeque;
use std::io::Write as _;
use std::ops::Range;
use std::time::{Duration, Instant};

use pepper_sync::config::{PerformanceLevel, SyncConfig};
use pepper_sync::events::{ScanTiming, SequencedSyncEvent, SyncEvent};
use tokio::sync::broadcast::error::RecvError;

use zingolib::config::{
    ChainType, ClientConfig, DEFAULT_INDEXER_URI, DEFAULT_INDEXER_URI_TESTNET, WalletConfig,
};
use zingolib::data::PollReport;
use zingolib::lightclient::LightClient;
use zingolib::wallet::WalletSettings;

const USAGE: &str =
    "usage: sync_events <UFVK> <BIRTHDAY> [--server <URI>] [--chain mainnet|testnet]";

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/// Indent applied to every batch line so it nests under the overall progress line.
const BATCH_INDENT: &str = "  ";

struct Args {
    ufvk: String,
    birthday: u32,
    server: http::Uri,
    chain: ChainType,
}

fn parse_args() -> Result<Args, String> {
    let mut positionals = Vec::new();
    let mut server = None;
    let mut chain = ChainType::Mainnet;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--server" => {
                let uri = args.next().ok_or("--server requires a URI")?;
                server = Some(uri.parse::<http::Uri>().map_err(|e| e.to_string())?);
            }
            "--chain" => {
                chain = match args.next().as_deref() {
                    Some("mainnet") => ChainType::Mainnet,
                    Some("testnet") => ChainType::Testnet,
                    other => return Err(format!("unsupported chain: {other:?}")),
                };
            }
            "--help" | "-h" => return Err(USAGE.to_string()),
            _ => positionals.push(arg),
        }
    }

    let [ufvk, birthday] = positionals.try_into().map_err(|_| USAGE.to_string())?;
    let birthday = birthday
        .parse()
        .map_err(|_| "birthday must be a block height")?;
    let server = match server {
        Some(uri) => uri,
        None => match chain {
            ChainType::Testnet => DEFAULT_INDEXER_URI_TESTNET.parse().expect("valid constant"),
            _ => DEFAULT_INDEXER_URI.parse().expect("valid constant"),
        },
    };

    Ok(Args {
        ufvk,
        birthday,
        server,
        chain,
    })
}

fn zec(zats: u64) -> String {
    format!("{}.{:08} ZEC", zats / 100_000_000, zats % 100_000_000)
}

fn group(n: u64) -> String {
    let digits = n.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(c);
    }
    grouped
}

fn bar(frac: f64, width: usize) -> String {
    let filled = ((frac.clamp(0.0, 1.0) * width as f64).round() as usize).min(width);
    format!("{}{}", "█".repeat(filled), "░".repeat(width - filled))
}

fn fmt_duration(seconds: u64) -> String {
    match seconds {
        s if s >= 3600 => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
        s if s >= 60 => format!("{}m{:02}s", s / 60, s % 60),
        s => format!("{s}s"),
    }
}

/// Formats a phase duration with millisecond resolution under a second, for the per-phase
/// breakdown on a finished batch line.
fn fmt_precise(duration: Duration) -> String {
    let seconds = duration.as_secs_f64();
    if seconds < 1.0 {
        format!("{}ms", duration.as_millis())
    } else if seconds < 60.0 {
        format!("{seconds:.1}s")
    } else {
        fmt_duration(seconds as u64)
    }
}

/// The committed result of a batch, recorded when `RangeScanned` arrives.
struct DoneInfo {
    outputs: u64,
    /// The engine's per-phase timing for the batch, including the commit.
    timing: ScanTiming,
    /// Time the batch spent queued behind the serialized commit stage, observed between
    /// `BatchScanCompleted` and `BatchCommitStarted`.
    waited: Duration,
}

/// The lifecycle phase of a batch, driven by the engine's lifecycle events.
enum Phase {
    /// Fetch, decryption, and tree construction are in progress (`BatchScanStarted`).
    Scanning,
    /// Scanning finished. The batch is blocked behind the serialized commit stage
    /// (`BatchScanCompleted`).
    Waiting { since: Instant },
    /// The batch holds the wallet lock and is committing (`BatchCommitStarted`).
    Committing { since: Instant, waited: Duration },
    /// The batch committed (`RangeScanned`). It lingers in the block until flushed to the log.
    Done(DoneInfo),
}

/// A batch the engine is processing, tracked from `BatchScanStarted` through its commit. The
/// `phase` advances as the engine's lifecycle events arrive, and a `Done` entry lingers in the
/// block until the next batch starts and flushes it into the scrollback log.
struct InFlightBatch {
    range: Range<u32>,
    priority: String,
    outputs: u64,
    /// When scanning started (`BatchScanStarted`), for the scanning bar.
    started: Instant,
    phase: Phase,
}

/// Terminal view: event lines print and scroll, a status block redraws in place underneath.
///
/// The block is one overall line plus one indented line per batch. A batch line shows live
/// progress while scanning and is marked DONE in place when it commits, staying in the block
/// next to any still-scanning sibling until the next batch starts and flushes it into the
/// scrollback log. Every status line is truncated to the terminal width: a wrapped status line
/// would occupy two physical rows and break the cursor movements that redraw the block.
///
/// A batch line reflects its lifecycle phase. While scanning it shows a bar estimated from the
/// measured scan rate (fetch plus decryption plus tree construction per output). When it
/// finishes scanning but is blocked behind the serialized commit stage it shows `WAITING FOR
/// OTHER TASKS`, with no bar, because it is making no progress. When it acquires the wallet lock
/// it shows a committing bar estimated from the measured commit rate. Each rate is averaged over
/// a window of recent batches from the engine's `ScanTiming` breakdown on `RangeScanned`. The
/// overall ETA is separate: it is the directly observed rate at which scanned outputs accumulate
/// in wall time, which already reflects however the work parallelises or serialises.
struct View {
    outputs_total: u64,
    outputs_scanned: u64,
    txs_found: u64,
    batches_done: u64,
    /// Batches the engine is processing, in start order.
    in_flight: Vec<InFlightBatch>,
    /// Recent committed batches as (outputs, per-phase timing), for the scan and commit rates.
    timing_log: VecDeque<(u64, ScanTiming)>,
    /// Recent commits as (time, cumulative outputs), for the overall wall-clock rate.
    progress_log: VecDeque<(Instant, u64)>,
    spinner_frame: usize,
    /// Physical rows the status block occupied at the last draw.
    status_rows: usize,
    term_width: usize,
}

const COMMIT_WINDOW: usize = 12;

/// Queries the controlling terminal for its width, defaulting to 100 columns.
fn terminal_width() -> usize {
    std::process::Command::new("stty")
        .arg("size")
        .stdin(std::process::Stdio::inherit())
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .and_then(|size| size.split_whitespace().nth(1)?.parse().ok())
        .or_else(|| std::env::var("COLUMNS").ok()?.parse().ok())
        .filter(|width| *width >= 40)
        .unwrap_or(100)
}

impl View {
    fn new() -> Self {
        Self {
            outputs_total: 0,
            outputs_scanned: 0,
            txs_found: 0,
            batches_done: 0,
            in_flight: Vec::new(),
            timing_log: VecDeque::with_capacity(COMMIT_WINDOW + 1),
            progress_log: VecDeque::with_capacity(COMMIT_WINDOW + 1),
            spinner_frame: 0,
            status_rows: 0,
            term_width: terminal_width(),
        }
    }

    /// Outputs per second of scan work (fetch, decryption, and tree construction) over the
    /// recent window, for the scanning bar.
    fn scan_throughput(&self) -> Option<f64> {
        self.rate(|timing| timing.fetch + timing.decryption + timing.tree)
    }

    /// Outputs per second of commit work over the recent window, for the committing bar.
    fn commit_throughput(&self) -> Option<f64> {
        self.rate(|timing| timing.commit.total())
    }

    /// Outputs per second for the phase duration `phase` extracts from each windowed batch.
    fn rate(&self, phase: impl Fn(&ScanTiming) -> Duration) -> Option<f64> {
        let outputs: u64 = self.timing_log.iter().map(|(outputs, _)| outputs).sum();
        let seconds: f64 = self
            .timing_log
            .iter()
            .map(|(_, timing)| phase(timing).as_secs_f64())
            .sum();
        (seconds > 0.0 && outputs > 0).then(|| outputs as f64 / seconds)
    }

    /// The directly observed rate (outputs per second) at which scanned outputs accumulate in
    /// wall time, for the overall ETA. Immune to how the work parallelises or serialises.
    fn aggregate_rate(&self) -> Option<f64> {
        let (first_at, first_outputs) = self.progress_log.front()?;
        let (last_at, last_outputs) = self.progress_log.back()?;
        let span = last_at.duration_since(*first_at).as_secs_f64();
        (span > 0.5 && last_outputs > first_outputs)
            .then(|| (last_outputs - first_outputs) as f64 / span)
    }

    /// Truncates a status line so it cannot wrap.
    fn fit(&self, line: String) -> String {
        let max = self.term_width.saturating_sub(1);
        if line.chars().count() <= max {
            line
        } else {
            line.chars().take(max).collect()
        }
    }

    fn overall_line(&self) -> String {
        let frac = if self.outputs_total > 0 {
            self.outputs_scanned as f64 / self.outputs_total as f64
        } else {
            0.0
        };
        let eta = match self.aggregate_rate() {
            Some(rate) if rate > 1.0 && self.outputs_total > self.outputs_scanned => {
                let seconds = (self.outputs_total - self.outputs_scanned) as f64 / rate;
                format!(" | ETA {}", fmt_duration(seconds as u64))
            }
            _ => String::new(),
        };
        format!(
            "[{}] {:5.1}%{eta} | {}/{} outputs | {} txs | {} batches",
            bar(frac, 24),
            frac * 100.0,
            group(self.outputs_scanned),
            group(self.outputs_total),
            self.txs_found,
            self.batches_done,
        )
    }

    fn batch_line(&self, batch: &InFlightBatch) -> String {
        let spinner = SPINNER[self.spinner_frame % SPINNER.len()];
        let Range { start, end } = batch.range;
        let priority = &batch.priority;
        match &batch.phase {
            Phase::Done(done) => {
                let timing = &done.timing;
                let commit = &timing.commit;
                // the commit breakdown leads (the suspect under investigation). it survives the
                // width truncation while pinned, and prints in full once flushed to history
                format!(
                    "{BATCH_INDENT}✓ scanned {start}..{end} [{priority}] ({} blocks, {} outputs) \
                     in {} · commit {} [ckpt {} · front {} · insert {} · spend_fetch {} · \
                     spend_cpu {} · cleanup {} · other {}] · wait {} · scan {}/{}/{}",
                    end - start,
                    group(done.outputs),
                    fmt_precise(done.waited + timing.total()),
                    fmt_precise(commit.total()),
                    fmt_precise(commit.checkpoints),
                    fmt_precise(commit.frontiers),
                    fmt_precise(commit.insert_tree),
                    fmt_precise(commit.spend_fetch),
                    fmt_precise(commit.spend_cpu),
                    fmt_precise(commit.cleanup),
                    fmt_precise(commit.other),
                    fmt_precise(done.waited),
                    fmt_precise(timing.fetch),
                    fmt_precise(timing.decryption),
                    fmt_precise(timing.tree),
                )
            }
            Phase::Scanning => {
                let estimate = self.phase_estimate(
                    batch.outputs,
                    batch.started.elapsed(),
                    self.scan_throughput(),
                );
                format!("{BATCH_INDENT}{spinner} scanning {start}..{end} [{priority}]{estimate}")
            }
            Phase::Waiting { since } => {
                format!(
                    "{BATCH_INDENT}{spinner} scanned {start}..{end} [{priority}] — WAITING FOR \
                     OTHER TASKS ({})",
                    fmt_duration(since.elapsed().as_secs()),
                )
            }
            Phase::Committing { since, .. } => {
                let estimate =
                    self.phase_estimate(batch.outputs, since.elapsed(), self.commit_throughput());
                format!("{BATCH_INDENT}{spinner} committing {start}..{end} [{priority}]{estimate}")
            }
        }
    }

    /// Renders a progress bar for a timed phase: `elapsed` against `outputs / rate`. Falls back
    /// to the bare output count when no rate is known yet, and to a full bar once a phase runs
    /// past the windowed average rather than freezing at a stale percent.
    fn phase_estimate(&self, outputs: u64, elapsed: Duration, rate: Option<f64>) -> String {
        match rate {
            Some(rate) if rate > 0.0 && outputs > 0 => {
                let expected = outputs as f64 / rate;
                let elapsed = elapsed.as_secs_f64();
                if elapsed >= expected {
                    format!(" [{}] finishing", bar(1.0, 10))
                } else {
                    let frac = elapsed / expected;
                    let left = expected - elapsed;
                    format!(
                        " [{}] ~{:3.0}% (~{} left)",
                        bar(frac, 10),
                        frac * 100.0,
                        fmt_duration(left.ceil() as u64),
                    )
                }
            }
            _ => format!(" | {} outputs", group(outputs)),
        }
    }

    fn status_lines(&self) -> Vec<String> {
        let mut lines = vec![self.fit(self.overall_line())];
        if self.in_flight.is_empty() {
            let spinner = SPINNER[self.spinner_frame % SPINNER.len()];
            lines.push(self.fit(format!("{spinner} waiting for scanner")));
        } else {
            lines.extend(
                self.in_flight
                    .iter()
                    .map(|batch| self.fit(self.batch_line(batch))),
            );
        }
        lines
    }

    fn draw_status(&mut self) {
        self.clear_status();
        let lines = self.status_lines();
        print!("{}", lines.join("\n"));
        let _ = std::io::stdout().flush();
        self.status_rows = lines.len();
    }

    fn clear_status(&mut self) {
        if self.status_rows > 0 {
            print!("\r\x1b[2K");
            for _ in 1..self.status_rows {
                print!("\x1b[1A\x1b[2K");
            }
            self.status_rows = 0;
        }
    }

    /// Prints a line above the status block.
    fn line(&mut self, text: &str) {
        self.clear_status();
        println!("{text}");
        self.draw_status();
    }

    /// Finds an in-flight batch by range that has not yet committed.
    fn live_batch(&mut self, range: &Range<u32>) -> Option<&mut InFlightBatch> {
        self.in_flight
            .iter_mut()
            .find(|batch| batch.range == *range && !matches!(batch.phase, Phase::Done(_)))
    }

    /// Moves the batch covering `range` from scanning into the waiting-for-commit phase.
    fn batch_scan_completed(&mut self, range: &Range<u32>) {
        if let Some(batch) = self.live_batch(range) {
            batch.phase = Phase::Waiting {
                since: Instant::now(),
            };
        }
    }

    /// Moves the batch covering `range` into the committing phase, capturing how long it waited.
    fn batch_commit_started(&mut self, range: &Range<u32>) {
        if let Some(batch) = self.live_batch(range) {
            let waited = match &batch.phase {
                Phase::Waiting { since } => since.elapsed(),
                _ => Duration::ZERO,
            };
            batch.phase = Phase::Committing {
                since: Instant::now(),
                waited,
            };
        }
    }

    /// Records a committed batch, feeding its per-phase timing into the rate windows and marking
    /// the matching in-flight entry DONE in place. Returns `false` if no entry matched the range
    /// (for example a refetch range, or one cleared by an intervening reorg).
    fn batch_committed(&mut self, range: &Range<u32>, outputs: u64, timing: ScanTiming) -> bool {
        self.outputs_scanned += outputs;
        self.batches_done += 1;
        self.timing_log.push_back((outputs, timing));
        if self.timing_log.len() > COMMIT_WINDOW {
            self.timing_log.pop_front();
        }
        self.progress_log
            .push_back((Instant::now(), self.outputs_scanned));
        if self.progress_log.len() > COMMIT_WINDOW {
            self.progress_log.pop_front();
        }
        match self.live_batch(range) {
            Some(batch) => {
                let waited = match &batch.phase {
                    Phase::Committing { waited, .. } => *waited,
                    Phase::Waiting { since } => since.elapsed(),
                    _ => Duration::ZERO,
                };
                batch.phase = Phase::Done(DoneInfo {
                    outputs,
                    timing,
                    waited,
                });
                true
            }
            None => false,
        }
    }

    /// Prints every settled DONE batch as a permanent scrollback line and drops it from the
    /// block. The caller redraws the block afterwards.
    fn flush_done(&mut self) {
        let lines: Vec<String> = self
            .in_flight
            .iter()
            .filter(|batch| matches!(batch.phase, Phase::Done(_)))
            .map(|batch| self.batch_line(batch))
            .collect();
        if lines.is_empty() {
            return;
        }
        self.clear_status();
        for line in lines {
            println!("{line}");
        }
        self.in_flight
            .retain(|batch| !matches!(batch.phase, Phase::Done(_)));
    }

    /// Records a batch handed to a scan worker, flushing any settled DONE batches into the log
    /// to bound the block and replacing a retried range's stale entry.
    fn batch_started(&mut self, batch: InFlightBatch) {
        self.flush_done();
        self.in_flight.retain(|other| other.range != batch.range);
        self.in_flight.push(batch);
    }

    fn tick(&mut self) {
        self.spinner_frame += 1;
        self.draw_status();
    }

    fn finish(&mut self) {
        // flush the last generation of DONE batches into the log, then erase the live block
        self.flush_done();
        self.clear_status();
    }
}

async fn handle_event(event: SequencedSyncEvent, view: &mut View, lc: &LightClient) {
    match event.event {
        SyncEvent::SessionStarted {
            sync_start_height,
            birthday,
            tip,
            total_sapling_outputs,
            total_orchard_outputs,
            total_ironwood_outputs,
            already_scanned_sapling_outputs,
            already_scanned_orchard_outputs,
            already_scanned_ironwood_outputs,
            ..
        } => {
            view.outputs_total =
                u64::from(total_sapling_outputs + total_orchard_outputs + total_ironwood_outputs);
            view.outputs_scanned = u64::from(
                already_scanned_sapling_outputs
                    + already_scanned_orchard_outputs
                    + already_scanned_ironwood_outputs,
            );
            view.timing_log.clear();
            view.progress_log.clear();
            view.progress_log
                .push_back((Instant::now(), view.outputs_scanned));
            view.line(&format!(
                "session started: birthday {birthday}, sync start {sync_start_height}, tip {tip}"
            ));
            view.line(&format!(
                "outputs in window: {} (sapling {} | orchard {} | ironwood {}), {} already scanned",
                group(view.outputs_total),
                group(u64::from(total_sapling_outputs)),
                group(u64::from(total_orchard_outputs)),
                group(u64::from(total_ironwood_outputs)),
                group(view.outputs_scanned),
            ));
        }
        SyncEvent::BatchScanStarted {
            range,
            priority,
            sapling_outputs,
            orchard_outputs,
            ironwood_outputs,
        } => {
            view.batch_started(InFlightBatch {
                range: u32::from(range.start)..u32::from(range.end),
                priority: format!("{priority:?}"),
                outputs: u64::from(sapling_outputs + orchard_outputs + ironwood_outputs),
                started: Instant::now(),
                phase: Phase::Scanning,
            });
            view.draw_status();
        }
        SyncEvent::BatchScanCompleted { range } => {
            view.batch_scan_completed(&(u32::from(range.start)..u32::from(range.end)));
            view.draw_status();
        }
        SyncEvent::BatchCommitStarted { range } => {
            view.batch_commit_started(&(u32::from(range.start)..u32::from(range.end)));
            view.draw_status();
        }
        SyncEvent::RangeScanned {
            range,
            priority,
            sapling_outputs,
            orchard_outputs,
            ironwood_outputs,
            timing,
        } => {
            let outputs = u64::from(sapling_outputs + orchard_outputs + ironwood_outputs);
            let range = u32::from(range.start)..u32::from(range.end);
            if view.batch_committed(&range, outputs, timing) {
                // the batch's own line now re-renders as DONE in place
                view.draw_status();
            } else {
                // no scanning entry matched (a refetch range, or one cleared by a reorg): log
                // the commit as a one-off line
                view.line(&format!(
                    "{BATCH_INDENT}✓ scanned {}..{} [{priority:?}] ({} blocks, {} outputs)",
                    range.start,
                    range.end,
                    range.end - range.start,
                    group(outputs),
                ));
            }
        }
        SyncEvent::TxDiscovered { txid, status } => {
            view.txs_found += 1;
            // the event is a hint: query the wallet for the committed transaction. Summaries
            // share the code path of the `transactions`/`value_transfers` views, so the kind
            // and amount match them (change outputs are not reported as received).
            let summary = lc.transaction_summary(txid).await.ok().flatten();
            let mut text = format!("tx {txid} [{status} {}]", status.get_height());
            if let Some(summary) = summary {
                text.push_str(&format!(" {} {}", summary.kind, zec(summary.value)));
                if let Some(fee) = summary.fee {
                    text.push_str(&format!(" (fee {})", zec(fee)));
                }
            }
            view.line(&text);
        }
        SyncEvent::Reorg { reverted_to } => {
            // a reverted verify batch never commits: drop any unpaired starts
            view.in_flight.clear();
            view.line(&format!("reorg! wallet data reverted to {reverted_to}"));
        }
        SyncEvent::TipMoved { to } => {
            view.line(&format!("tip moved to {to}"));
        }
    }
}

/// On a lagged stream, reconcile counters against wallet state (SYNC_UX_SPEC.md §7.2).
async fn reconcile(view: &mut View, lc: &LightClient, skipped: u64) {
    let wallet = lc.wallet().read().await;
    if let Ok(status) = pepper_sync::sync_status(&*wallet).await {
        view.outputs_scanned = u64::from(
            status.total_sapling_outputs_scanned
                + status.total_orchard_outputs_scanned
                + status.total_ironwood_outputs_scanned,
        );
        // skipped commits left no measured samples: drop the windows and rebuild them from the
        // next batches that commit
        view.timing_log.clear();
        view.progress_log.clear();
        view.progress_log
            .push_back((Instant::now(), view.outputs_scanned));
    }
    drop(wallet);
    view.line(&format!(
        "event stream lagged ({skipped} events skipped), reconciled from wallet state"
    ));
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = match parse_args() {
        Ok(args) => args,
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    };

    // throwaway wallet dir: nothing is saved since the save task is never started
    let wallet_dir = std::env::temp_dir().join(format!("zingo-sync-events-{}", std::process::id()));
    std::fs::create_dir_all(&wallet_dir)?;

    let config = ClientConfig::builder()
        .set_chain_type(args.chain)
        .set_indexer_uri(args.server.clone())
        .set_wallet_config(WalletConfig::Ufvk {
            ufvk: args.ufvk,
            birthday: args.birthday,
            wallet_settings: WalletSettings {
                sync_config: SyncConfig {
                    performance_level: PerformanceLevel::Maximum,
                    ..SyncConfig::default()
                },
                ..WalletSettings::default()
            },
        })
        .set_wallet_dir(wallet_dir)
        .build();

    println!("connecting to {}", args.server);
    let mut lc = LightClient::new(config, true, None).await?;

    // subscribe before launching so SessionStarted is not missed
    let mut events = lc.subscribe_sync_events();
    lc.sync().await?;

    let mut view = View::new();
    let mut poll_interval = tokio::time::interval(Duration::from_millis(500));
    poll_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut render_interval = tokio::time::interval(Duration::from_millis(100));
    render_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut stopping = false;

    let sync_result = loop {
        tokio::select! {
            event = events.recv() => match event {
                Ok(event) => handle_event(event, &mut view, &lc).await,
                Err(RecvError::Lagged(skipped)) => reconcile(&mut view, &lc, skipped).await,
                Err(RecvError::Closed) => break Err("event stream closed".into()),
            },

            _ = render_interval.tick() => view.tick(),

            _ = poll_interval.tick() => match lc.poll_sync() {
                PollReport::Ready(result) => {
                    // drain events emitted between the last recv and completion
                    while let Ok(event) = events.try_recv() {
                        handle_event(event, &mut view, &lc).await;
                    }
                    break result.map_err(Into::into);
                }
                PollReport::NotReady => {}
                PollReport::NoHandle => break Err("sync task disappeared".into()),
            },

            _ = tokio::signal::ctrl_c() => {
                if stopping {
                    view.finish();
                    std::process::exit(130);
                }
                stopping = true;
                // a failed stop means sync already finished, and the poll arm picks up the result
                let _already_finished = lc.stop_sync();
                view.line("stopping after current batch (Ctrl-C again to abort)...");
            }
        }
    };

    view.finish();
    match sync_result {
        Ok(result) => {
            println!("\n{result}");
            println!("{}", lc.account_balance(zip32::AccountId::ZERO).await?);
            Ok(())
        }
        Err(e) => Err(e),
    }
}
