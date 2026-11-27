//! Wall-clock benchmark of pepper-sync against fabricated chains served by the mock indexer.
//!
//! Every scenario builds a chain in process, syncs a fresh wallet from its birthday, mines
//! more blocks of the same shape and syncs again, then rescans from scratch. Chains carry
//! proof-free shielded outputs, so generation is cheap and the time measured is the wallet's:
//! fetching, trial decryption, witness commits and spend bookkeeping.
//!
//! The sync task is polled every millisecond, so the harness adds at most that much to a
//! measurement regardless of how the client waits for completion.
//!
//! Each client connects through a transport that counts bytes, so each measurement also has the
//! requests for each kind and the bytes on the wire.
//!
//! Run with `cargo bench -p ekdromos --bench pepper_sync -- [name filter...]`.
//!
//! Environment:
//! - `PEPPER_BENCH_BLOCKS`: blocks per chain (default 3000). The incremental phase mines a tenth more.
//! - `PEPPER_BENCH_RUNS`: fresh wallets per scenario (default 3). The table reports the minimum.
//! - `PEPPER_BENCH_OUT`: path of a JSON lines file that receives every measurement.
//! - `PEPPER_BENCH_VARIANTS`: comma-separated variant names to keep (default all).

use std::collections::BTreeMap;
use std::io::Write as _;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use chain_router::{
    AllowedIdentifiers, BoxFuture, Connection, Connector, Integrity, Quorum, RequestEvent,
    TransportInfo, TxidRule,
};
use ekdromos::config::{Birthday, WalletConfig};
use ekdromos::data::PollReport;
use ekdromos::lightclient::store::MemoryStore;
use ekdromos::lightclient::{Encryption, LightClient};
use ekdromos::testutils::default_test_wallet_settings;
use ekdromos::testutils::mock_indexer::{
    CoinbaseRecipient, MockNet, shielded_coinbase_transaction,
};
use ekdromos::testutils::synthetic_wallet::SyntheticWalletBuilder;
use ekdromos::wallet::keys::unified::ReceiverSelection;
use lightwallet_grpc_client::GrpcIndexer;
use pepper_sync::config::{PerformanceLevel, SyncConfig, TransparentAddressDiscovery};
use pepper_sync::events::{SequencedSyncEvent, SyncEvent};
use pepper_sync::sync::SyncResult;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpStream;
use zcash_protocol::value::Zatoshis;

const WALLET_SEED: &str = ekdromos_test_vectors::seeds::HOSPITAL_MUSEUM_SEED;
const FOREIGN_SEED: &str = ekdromos_test_vectors::seeds::ABANDON_ART_SEED;
const NOISE_VALUE: Zatoshis = Zatoshis::const_from_u64(1_000);
const WALLET_NOTE_VALUE: Zatoshis = Zatoshis::const_from_u64(100_000);
const COINBASE_REWARD: Zatoshis = Zatoshis::const_from_u64(625_000_000);

#[derive(Clone, Copy, Debug)]
enum Pool {
    Sapling,
    Ironwood,
}

/// What every block of a chain carries.
#[derive(Clone, Copy, Debug)]
struct Shape {
    sapling_noise: u32,
    ironwood_noise: u32,
    transparent_coinbase: bool,
    /// A note to the wallet in the given pool every n blocks.
    wallet_note: Option<(Pool, u32)>,
    /// The transparent coinbase pays the wallet every n blocks.
    wallet_coinbase_every: Option<u32>,
}

impl Shape {
    const EMPTY: Self = Self {
        sapling_noise: 0,
        ironwood_noise: 0,
        transparent_coinbase: true,
        wallet_note: None,
        wallet_coinbase_every: None,
    };
    const MIXED: Self = Self {
        sapling_noise: 5,
        ironwood_noise: 5,
        transparent_coinbase: true,
        wallet_note: None,
        wallet_coinbase_every: None,
    };
}

/// The sources of a client and what they can receive.
#[derive(Clone, Copy, Debug)]
enum Sources {
    /// One indexer that can receive each wallet identifier.
    Direct,
    /// One indexer that can receive a txid only among this number of txids.
    Decoys(usize),
    /// Two unverified indexers with a quorum router.
    Quorum,
}

#[derive(Clone, Copy, Debug)]
struct Variant {
    name: &'static str,
    performance: PerformanceLevel,
    discovery: fn() -> TransparentAddressDiscovery,
    sources: Sources,
}

impl Variant {
    const DEFAULT: Self = Self {
        name: "high/minimal",
        performance: PerformanceLevel::High,
        discovery: TransparentAddressDiscovery::minimal,
        sources: Sources::Direct,
    };

    const fn with_sources(name: &'static str, sources: Sources) -> Self {
        Self {
            name,
            sources,
            ..Self::DEFAULT
        }
    }
}

/// The requests and the bytes of one client.
#[derive(Clone, Default)]
struct Traffic {
    received: Arc<AtomicU64>,
    sent: Arc<AtomicU64>,
    requests: Arc<Mutex<BTreeMap<String, u64>>>,
}

impl Traffic {
    /// Returns the totals since the last call and sets them to zero.
    fn take(&self) -> TrafficTotals {
        TrafficTotals {
            received: self.received.swap(0, Ordering::Relaxed),
            sent: self.sent.swap(0, Ordering::Relaxed),
            requests: std::mem::take(&mut *self.requests.lock().expect("no panic with the lock")),
        }
    }
}

struct TrafficTotals {
    received: u64,
    sent: u64,
    requests: BTreeMap<String, u64>,
}

struct CountedStream {
    stream: TcpStream,
    traffic: Traffic,
}

impl AsyncRead for CountedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buffer.filled().len();
        let poll = Pin::new(&mut self.stream).poll_read(context, buffer);
        let read = (buffer.filled().len() - before) as u64;
        self.traffic.received.fetch_add(read, Ordering::Relaxed);
        poll
    }
}

impl AsyncWrite for CountedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let poll = Pin::new(&mut self.stream).poll_write(context, bytes);
        if let Poll::Ready(Ok(written)) = poll {
            self.traffic
                .sent
                .fetch_add(written as u64, Ordering::Relaxed);
        }
        poll
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(context)
    }
}

struct CountingTcp(Traffic);

impl Connector for CountingTcp {
    fn connect<'a>(
        &'a self,
        host: &'a str,
        port: u16,
    ) -> BoxFuture<'a, std::io::Result<Box<dyn Connection>>> {
        Box::pin(async move {
            let stream = TcpStream::connect((host, port)).await?;
            let traffic = self.0.clone();
            Ok(Box::new(CountedStream { stream, traffic }) as Box<dyn Connection>)
        })
    }

    fn info(&self) -> TransportInfo {
        TransportInfo::direct()
    }
}

#[derive(Clone)]
struct Scenario {
    name: &'static str,
    shape: Shape,
    variants: Vec<Variant>,
}

fn scenarios() -> Vec<Scenario> {
    let levels = [
        ("low/minimal", PerformanceLevel::Low),
        ("medium/minimal", PerformanceLevel::Medium),
        ("high/minimal", PerformanceLevel::High),
        ("maximum/minimal", PerformanceLevel::Maximum),
    ]
    .into_iter()
    .map(|(name, performance)| Variant {
        name,
        performance,
        ..Variant::DEFAULT
    })
    .collect();
    let discoveries = vec![
        Variant {
            name: "high/disabled",
            discovery: TransparentAddressDiscovery::disabled,
            ..Variant::DEFAULT
        },
        Variant::DEFAULT,
        Variant {
            name: "high/recovery",
            discovery: TransparentAddressDiscovery::recovery,
            ..Variant::DEFAULT
        },
    ];
    let sources = vec![
        Variant::with_sources("direct", Sources::Direct),
        Variant::with_sources("decoys/16", Sources::Decoys(16)),
        Variant::with_sources("decoys/64", Sources::Decoys(64)),
        Variant::with_sources("decoys/256", Sources::Decoys(256)),
        Variant::with_sources("quorum", Sources::Quorum),
    ];

    vec![
        Scenario {
            name: "empty_blocks",
            shape: Shape::EMPTY,
            variants: vec![Variant::DEFAULT],
        },
        Scenario {
            name: "sapling_noise_10",
            shape: Shape {
                sapling_noise: 10,
                transparent_coinbase: false,
                ..Shape::EMPTY
            },
            variants: vec![Variant::DEFAULT],
        },
        Scenario {
            name: "ironwood_noise_10",
            shape: Shape {
                ironwood_noise: 10,
                transparent_coinbase: false,
                ..Shape::EMPTY
            },
            variants: levels,
        },
        Scenario {
            name: "mixed_noise",
            shape: Shape::MIXED,
            variants: vec![Variant::DEFAULT],
        },
        Scenario {
            name: "wallet_sapling_notes",
            shape: Shape {
                wallet_note: Some((Pool::Sapling, 10)),
                ..Shape::MIXED
            },
            variants: vec![Variant::DEFAULT],
        },
        Scenario {
            name: "wallet_ironwood_notes",
            shape: Shape {
                wallet_note: Some((Pool::Ironwood, 10)),
                ..Shape::MIXED
            },
            variants: vec![Variant::DEFAULT],
        },
        Scenario {
            name: "wallet_ironwood_dense",
            shape: Shape {
                sapling_noise: 1,
                ironwood_noise: 1,
                wallet_note: Some((Pool::Ironwood, 1)),
                ..Shape::MIXED
            },
            variants: vec![Variant::DEFAULT],
        },
        Scenario {
            name: "wallet_transparent",
            shape: Shape {
                wallet_coinbase_every: Some(10),
                ..Shape::MIXED
            },
            variants: discoveries,
        },
        Scenario {
            name: "sources_sparse_wallet",
            shape: Shape {
                wallet_note: Some((Pool::Ironwood, 100)),
                ..Shape::MIXED
            },
            variants: sources.clone(),
        },
        Scenario {
            name: "sources_dense_wallet",
            shape: Shape {
                wallet_note: Some((Pool::Ironwood, 1)),
                ..Shape::MIXED
            },
            variants: sources,
        },
    ]
}

struct Addresses {
    sapling: sapling_crypto::PaymentAddress,
    orchard: orchard::Address,
    transparent: String,
}

fn addresses(seed: &str) -> Addresses {
    let mut wallet = SyntheticWalletBuilder::new(seed).build();
    let (_, sapling_only) = wallet
        .generate_unified_address(ReceiverSelection::sapling_only(), zip32::AccountId::ZERO)
        .expect("a sapling address derives");
    let orchard = *wallet
        .unified_addresses()
        .values()
        .find_map(|address| address.orchard())
        .expect("the first unified address has an orchard receiver");
    let transparent = wallet
        .transparent_addresses()
        .values()
        .next()
        .expect("the wallet starts with a transparent address")
        .clone();
    Addresses {
        sapling: *sapling_only
            .sapling()
            .expect("a sapling-only address has a sapling receiver"),
        orchard,
        transparent,
    }
}

async fn mine(net: &MockNet, shape: &Shape, wallet: &Addresses, foreign: &Addresses, count: u32) {
    let chain_type = net.chain_type();
    let mut chain = net.chain.write().await;
    for _ in 0..count {
        let height = chain.next_height();
        let index = u32::from(height);
        let capacity = (shape.sapling_noise + shape.ironwood_noise + 1) as usize;
        let mut transactions = Vec::with_capacity(capacity);
        for _ in 0..shape.sapling_noise {
            transactions.push(shielded_coinbase_transaction(
                &chain_type,
                height,
                CoinbaseRecipient::Sapling(foreign.sapling),
                NOISE_VALUE,
            ));
        }
        for _ in 0..shape.ironwood_noise {
            transactions.push(shielded_coinbase_transaction(
                &chain_type,
                height,
                CoinbaseRecipient::Orchard(foreign.orchard),
                NOISE_VALUE,
            ));
        }
        if let Some((pool, every)) = shape.wallet_note
            && index % every == 0
        {
            let recipient = match pool {
                Pool::Sapling => CoinbaseRecipient::Sapling(wallet.sapling),
                Pool::Ironwood => CoinbaseRecipient::Orchard(wallet.orchard),
            };
            transactions.push(shielded_coinbase_transaction(
                &chain_type,
                height,
                recipient,
                WALLET_NOTE_VALUE,
            ));
        }
        if shape.transparent_coinbase {
            let miner = match shape.wallet_coinbase_every {
                Some(every) if index % every == 0 => &wallet.transparent,
                _ => &foreign.transparent,
            };
            chain.mine_block_rewarding(miner, COINBASE_REWARD, transactions);
        } else {
            chain.mine_block(transactions);
        }
    }
}

async fn client(net: &MockNet, variant: &Variant) -> (LightClient, Traffic) {
    let traffic = Traffic::default();
    let indexer = |integrity, identifiers: AllowedIdentifiers| {
        GrpcIndexer::with_connector(net.indexer_uri(), CountingTcp(traffic.clone()))
            .expect("the mock indexer URI is valid")
            .with_integrity(integrity)
            .allow_identifiers(identifiers)
    };
    let builder = LightClient::builder(net.chain_type());
    let builder = match variant.sources {
        Sources::Direct => builder.source(indexer(Integrity::Assumed, AllowedIdentifiers::all())),
        Sources::Decoys(minimum) => {
            let identifiers = AllowedIdentifiers {
                txid: TxidRule::Among(minimum),
                ..AllowedIdentifiers::all()
            };
            builder.source(indexer(Integrity::Assumed, identifiers))
        }
        Sources::Quorum => builder
            .source(indexer(Integrity::Unverified, AllowedIdentifiers::all()))
            .source(indexer(Integrity::Unverified, AllowedIdentifiers::all()))
            .router(Quorum::new(2)),
    };
    let requests = traffic.requests.clone();
    let builder = builder.request_events(move |event: &RequestEvent| {
        let mut requests = requests.lock().expect("no panic with the lock");
        *requests.entry(format!("{:?}", event.kind)).or_default() += 1;
    });

    let mut wallet_settings = default_test_wallet_settings();
    wallet_settings.sync_config = SyncConfig {
        transparent_address_discovery: (variant.discovery)(),
        performance_level: variant.performance,
        ..SyncConfig::default()
    };
    let wallet_config = WalletConfig::MnemonicPhrase {
        mnemonic_phrase: WALLET_SEED.to_string(),
        no_of_accounts: 1.try_into().expect("one is not zero"),
        birthday: Birthday::Height(1),
        wallet_settings,
    };
    let client = builder
        .create(MemoryStore::default(), wallet_config, Encryption::None)
        .await
        .expect("the client for the mock chain builds");
    traffic.take();
    (client, traffic)
}

struct Measurement {
    scenario: &'static str,
    variant: &'static str,
    phase: &'static str,
    run: usize,
    elapsed: Duration,
    result: SyncResult,
    traffic: TrafficTotals,
}

impl Measurement {
    fn json(&self, blocks: u32) -> String {
        serde_json::json!({
            "scenario": self.scenario,
            "variant": self.variant,
            "phase": self.phase,
            "run": self.run,
            "chain_blocks": blocks,
            "seconds": self.elapsed.as_secs_f64(),
            "blocks_scanned": self.result.blocks_scanned,
            "sapling_outputs": self.result.sapling_outputs_scanned,
            "orchard_outputs": self.result.orchard_outputs_scanned,
            "ironwood_outputs": self.result.ironwood_outputs_scanned,
            "bytes_received": self.traffic.received,
            "bytes_sent": self.traffic.sent,
            "requests": self.traffic.requests,
        })
        .to_string()
    }
}

async fn timed(client: &mut LightClient, rescan: bool) -> (Duration, SyncResult) {
    let mut events = client.subscribe_sync_events();
    let start = Instant::now();
    if rescan {
        client.rescan().await
    } else {
        client.sync().await
    }
    .expect("the mock chain sync starts");
    loop {
        match client.poll_sync() {
            PollReport::NoHandle => panic!("the sync task vanished"),
            PollReport::NotReady => tokio::time::sleep(Duration::from_millis(1)).await,
            PollReport::Ready(result) => {
                let elapsed = start.elapsed();
                report_phases(&mut events, elapsed);
                return (elapsed, result.expect("the mock chain syncs"));
            }
        }
    }
}

/// Sums the phase timings of every range the session committed and prints them beside the
/// wall-clock time. The remainder is session start (transparent discovery, re-org detection) and
/// time the scanner spent idle.
fn report_phases(
    events: &mut tokio::sync::broadcast::Receiver<SequencedSyncEvent>,
    elapsed: Duration,
) {
    let mut phases = [Duration::ZERO; 10];
    let mut ranges = 0u32;
    let mut lagged = 0u64;
    loop {
        match events.try_recv() {
            Ok(sequenced) => {
                if let SyncEvent::RangeScanned { timing, .. } = sequenced.event {
                    ranges += 1;
                    let commit = timing.commit;
                    for (sum, phase) in phases.iter_mut().zip([
                        timing.fetch,
                        timing.decryption,
                        timing.tree,
                        commit.checkpoints,
                        commit.frontiers,
                        commit.insert_tree,
                        commit.spend_fetch,
                        commit.spend_cpu,
                        commit.cleanup,
                        commit.other,
                    ]) {
                        *sum += phase;
                    }
                }
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Lagged(n)) => lagged += n,
            Err(_) => break,
        }
    }
    let attributed = phases.iter().sum::<Duration>();
    let names = [
        "fetch",
        "decrypt",
        "tree",
        "checkpoints",
        "frontiers",
        "insert",
        "spend_fetch",
        "spend_cpu",
        "cleanup",
        "other",
    ];
    let breakdown = names
        .iter()
        .zip(phases)
        .map(|(name, phase)| format!("{name} {:.2}", phase.as_secs_f64()))
        .collect::<Vec<_>>()
        .join(" ");
    eprintln!(
        "    phases ({ranges} ranges): {breakdown} | unattributed {:.2}{}",
        elapsed.saturating_sub(attributed).as_secs_f64(),
        if lagged > 0 {
            format!(" (lagged {lagged} events)")
        } else {
            String::new()
        }
    );
}

async fn run_scenario(
    scenario: &Scenario,
    blocks: u32,
    runs: usize,
    wallet: &Addresses,
    foreign: &Addresses,
) -> Vec<Measurement> {
    let net = MockNet::launch().await;
    let generation = Instant::now();
    mine(&net, &scenario.shape, wallet, foreign, blocks).await;
    eprintln!(
        "{}: {blocks} blocks generated in {:.1}s",
        scenario.name,
        generation.elapsed().as_secs_f64()
    );

    let mut measurements = Vec::new();
    let mut clients = Vec::new();
    for variant in &scenario.variants {
        for run in 0..runs {
            let (mut client, traffic) = client(&net, variant).await;
            let (elapsed, result) = timed(&mut client, false).await;
            if run == 0 {
                let balance = client
                    .account_balance(zip32::AccountId::ZERO)
                    .await
                    .expect("the synced wallet reports a balance");
                eprintln!(
                    "  {} {} balance after full sync: {balance}",
                    scenario.name, variant.name
                );
            }
            eprintln!(
                "  {} {} full #{run}: {:.2}s ({result})",
                scenario.name,
                variant.name,
                elapsed.as_secs_f64()
            );
            measurements.push(Measurement {
                scenario: scenario.name,
                variant: variant.name,
                phase: "full",
                run,
                elapsed,
                result,
                traffic: traffic.take(),
            });
            clients.push((variant, run, client, traffic));
        }
    }

    mine(&net, &scenario.shape, wallet, foreign, blocks / 10).await;
    for (variant, run, client, traffic) in &mut clients {
        let (elapsed, result) = timed(client, false).await;
        eprintln!(
            "  {} {} incremental #{run}: {:.2}s ({result})",
            scenario.name,
            variant.name,
            elapsed.as_secs_f64()
        );
        measurements.push(Measurement {
            scenario: scenario.name,
            variant: variant.name,
            phase: "incremental",
            run: *run,
            elapsed,
            result,
            traffic: traffic.take(),
        });
    }
    for (variant, run, client, traffic) in &mut clients {
        let (elapsed, result) = timed(client, true).await;
        eprintln!(
            "  {} {} rescan #{run}: {:.2}s ({result})",
            scenario.name,
            variant.name,
            elapsed.as_secs_f64()
        );
        measurements.push(Measurement {
            scenario: scenario.name,
            variant: variant.name,
            phase: "rescan",
            run: *run,
            elapsed,
            result,
            traffic: traffic.take(),
        });
    }
    measurements
}

fn env_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(default)
}

fn main() {
    let blocks = env_or("PEPPER_BENCH_BLOCKS", 3_000u32);
    let runs = env_or("PEPPER_BENCH_RUNS", 3usize);
    let filters = std::env::args()
        .skip(1)
        .filter(|argument| !argument.starts_with("--"))
        .collect::<Vec<_>>();
    let mut output = std::env::var("PEPPER_BENCH_OUT")
        .ok()
        .map(|path| std::fs::File::create(&path).expect("the output path is writable"));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a tokio runtime builds");
    let wallet = addresses(WALLET_SEED);
    let foreign = addresses(FOREIGN_SEED);

    let mut all = Vec::new();
    let variant_filter: Option<Vec<String>> =
        std::env::var("PEPPER_BENCH_VARIANTS").ok().map(|list| {
            list.split(',')
                .map(|name| name.trim().to_string())
                .collect()
        });
    for mut scenario in scenarios() {
        if !filters.is_empty() && !filters.iter().any(|filter| scenario.name.contains(filter)) {
            continue;
        }
        if let Some(keep) = &variant_filter {
            scenario
                .variants
                .retain(|variant| keep.iter().any(|name| name == &variant.name));
            if scenario.variants.is_empty() {
                continue;
            }
        }
        let measurements =
            runtime.block_on(run_scenario(&scenario, blocks, runs, &wallet, &foreign));
        if let Some(output) = output.as_mut() {
            for measurement in &measurements {
                writeln!(output, "{}", measurement.json(blocks))
                    .expect("the output file is writable");
            }
        }
        all.extend(measurements);
    }

    println!(
        "\n| scenario | variant | phase | blocks | outputs (s/o/i) | best s | median s | requests | MiB received | MiB sent |"
    );
    println!("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |");
    let mut keys = all
        .iter()
        .map(|m| (m.scenario, m.variant, m.phase))
        .collect::<Vec<_>>();
    keys.dedup();
    for (scenario, variant, phase) in keys {
        let mut seconds = all
            .iter()
            .filter(|m| m.scenario == scenario && m.variant == variant && m.phase == phase)
            .map(|m| m.elapsed.as_secs_f64())
            .collect::<Vec<_>>();
        seconds.sort_by(|a, b| a.total_cmp(b));
        let sample = all
            .iter()
            .find(|m| m.scenario == scenario && m.variant == variant && m.phase == phase)
            .expect("the key came from this list");
        const MIB: f64 = 1024.0 * 1024.0;
        println!(
            "| {scenario} | {variant} | {phase} | {} | {}/{}/{} | {:.2} | {:.2} | {} | {:.2} | {:.2} |",
            sample.result.blocks_scanned,
            sample.result.sapling_outputs_scanned,
            sample.result.orchard_outputs_scanned,
            sample.result.ironwood_outputs_scanned,
            seconds[0],
            seconds[seconds.len() / 2],
            sample.traffic.requests.values().sum::<u64>(),
            sample.traffic.received as f64 / MIB,
            sample.traffic.sent as f64 / MIB,
        );
    }
}
