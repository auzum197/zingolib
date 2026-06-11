//! Watch the sync event stream for a view-only wallet.
//!
//! Creates a throwaway wallet from a UFVK, syncs it, and prints transactions the moment the
//! engine commits them. Each committed batch prints a line; the live status block shows overall
//! progress with an ETA plus the in-flight scan range with an estimated batch bar and spinner.
//! Progress is computed from the event stream (the consumer-side recipe from SYNC_UX_SPEC.md
//! §7.3); the in-flight range comes from a non-blocking peek at the wallet's scan state.
//! Nothing is written to disk.
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
use pepper_sync::events::{SequencedSyncEvent, SyncEvent};
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

/// A batch announced by `BatchScanStarted` and not yet committed by `RangeScanned`.
struct InFlightBatch {
    range: Range<u32>,
    priority: String,
    outputs: u64,
    started: Instant,
}

/// Terminal view: event lines print and scroll, a status block redraws in place underneath.
///
/// The block is one overall line plus one line per in-flight batch. Every status line is
/// truncated to the terminal width: a wrapped status line would occupy two physical rows and
/// break the cursor movements that redraw the block.
///
/// Batch timing is `announced outputs / throughput`: `BatchScanStarted` carries the exact
/// output count of the batch a worker just took, and scan time is dominated by trial
/// decryption, so outputs are the unit of work. Throughput is the only estimated term,
/// measured over a window of commits (cumulative outputs vs wall time). The workers share the
/// decryption thread pool, so a batch's own rate is the aggregate divided by the number of
/// batches in flight.
struct View {
    outputs_total: u64,
    outputs_scanned: u64,
    txs_found: u64,
    batches_done: u64,
    /// Started batches awaiting their commit, in start order.
    in_flight: Vec<InFlightBatch>,
    /// Recent commits as (commit time, cumulative outputs), for windowed throughput.
    commit_log: VecDeque<(Instant, u64)>,
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
            commit_log: VecDeque::with_capacity(COMMIT_WINDOW + 1),
            spinner_frame: 0,
            status_rows: 0,
            term_width: terminal_width(),
        }
    }

    /// Outputs per second over the recent commit window.
    fn throughput(&self) -> Option<f64> {
        let (first_at, first_outputs) = self.commit_log.front()?;
        let (last_at, last_outputs) = self.commit_log.back()?;
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
        let eta = match self.throughput() {
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
        let target = format!(
            "scanning {}..{} [{}]",
            batch.range.start, batch.range.end, batch.priority
        );
        // the workers share the decryption pool: each in-flight batch progresses at
        // roughly the aggregate rate split between them
        let batch_rate = self
            .throughput()
            .map(|rate| rate / self.in_flight.len().max(1) as f64);
        let estimate = match batch_rate {
            Some(rate) if rate > 0.0 && batch.outputs > 0 => {
                let expected = batch.outputs as f64 / rate;
                let elapsed = batch.started.elapsed().as_secs_f64();
                let frac = (elapsed / expected).min(0.99);
                let left = (expected - elapsed).max(0.0);
                format!(
                    " [{}] ~{:3.0}% (~{} left)",
                    bar(frac, 10),
                    frac * 100.0,
                    fmt_duration(left.ceil() as u64),
                )
            }
            _ => format!(" | {} outputs", group(batch.outputs)),
        };
        format!("{spinner} {target}{estimate}")
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

    /// Records a committed batch, updating the throughput window and retiring its
    /// in-flight entry.
    fn batch_committed(&mut self, range: &Range<u32>, outputs: u64) {
        self.outputs_scanned += outputs;
        self.batches_done += 1;
        self.commit_log
            .push_back((Instant::now(), self.outputs_scanned));
        if self.commit_log.len() > COMMIT_WINDOW {
            self.commit_log.pop_front();
        }
        self.in_flight.retain(|batch| batch.range != *range);
    }

    /// Records a batch handed to a scan worker, replacing a retried range's stale entry.
    fn batch_started(&mut self, batch: InFlightBatch) {
        self.in_flight.retain(|other| other.range != batch.range);
        self.in_flight.push(batch);
    }

    fn tick(&mut self) {
        self.spinner_frame += 1;
        self.draw_status();
    }

    fn finish(&mut self) {
        if self.status_rows > 0 {
            println!();
            self.status_rows = 0;
        }
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
            already_scanned_sapling_outputs,
            already_scanned_orchard_outputs,
            ..
        } => {
            view.outputs_total = u64::from(total_sapling_outputs + total_orchard_outputs);
            view.outputs_scanned =
                u64::from(already_scanned_sapling_outputs + already_scanned_orchard_outputs);
            view.commit_log.clear();
            view.commit_log
                .push_back((Instant::now(), view.outputs_scanned));
            view.line(&format!(
                "session started: birthday {birthday}, sync start {sync_start_height}, tip {tip}"
            ));
            view.line(&format!(
                "outputs in window: {} (sapling {} | orchard {}), {} already scanned",
                group(view.outputs_total),
                group(u64::from(total_sapling_outputs)),
                group(u64::from(total_orchard_outputs)),
                group(view.outputs_scanned),
            ));
        }
        SyncEvent::BatchScanStarted {
            range,
            priority,
            sapling_outputs,
            orchard_outputs,
        } => {
            view.batch_started(InFlightBatch {
                range: u32::from(range.start)..u32::from(range.end),
                priority: format!("{priority:?}"),
                outputs: u64::from(sapling_outputs + orchard_outputs),
                started: Instant::now(),
            });
            view.draw_status();
        }
        SyncEvent::RangeScanned {
            range,
            priority,
            sapling_outputs,
            orchard_outputs,
        } => {
            let outputs = u64::from(sapling_outputs + orchard_outputs);
            let range = u32::from(range.start)..u32::from(range.end);
            view.batch_committed(&range, outputs);
            view.line(&format!(
                "scanned {}..{} [{priority:?}] ({} blocks, {} outputs)",
                range.start,
                range.end,
                range.end - range.start,
                group(outputs),
            ));
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
        view.outputs_scanned =
            u64::from(status.total_sapling_outputs_scanned + status.total_orchard_outputs_scanned);
        // the cumulative jump would read as a throughput spike: restart the window
        view.commit_log.clear();
        view.commit_log
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
                // a failed stop means sync already finished; the poll arm picks up the result
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
