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
//! Run with `cargo bench -p zingolib --bench pepper_sync -- [name filter...]`.
//!
//! Environment:
//! - `PEPPER_BENCH_BLOCKS`: blocks per chain (default 3000). The incremental phase mines a tenth more.
//! - `PEPPER_BENCH_RUNS`: fresh wallets per scenario (default 3). The table reports the minimum.
//! - `PEPPER_BENCH_OUT`: path of a JSON lines file that receives every measurement.

use std::io::Write as _;
use std::time::{Duration, Instant};

use pepper_sync::config::{PerformanceLevel, SyncConfig, TransparentAddressDiscovery};
use pepper_sync::sync::SyncResult;
use zcash_protocol::value::Zatoshis;
use zingolib::data::PollReport;
use zingolib::lightclient::LightClient;
use zingolib::testutils::mock_indexer::{
    CoinbaseRecipient, MockNet, shielded_coinbase_transaction,
};
use zingolib::testutils::synthetic_wallet::SyntheticWalletBuilder;
use zingolib::wallet::keys::unified::ReceiverSelection;

const WALLET_SEED: &str = zingo_test_vectors::seeds::HOSPITAL_MUSEUM_SEED;
const FOREIGN_SEED: &str = zingo_test_vectors::seeds::ABANDON_ART_SEED;
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

#[derive(Clone, Copy, Debug)]
struct Variant {
    name: &'static str,
    performance: PerformanceLevel,
    discovery: fn() -> TransparentAddressDiscovery,
}

impl Variant {
    const DEFAULT: Self = Self {
        name: "high/minimal",
        performance: PerformanceLevel::High,
        discovery: TransparentAddressDiscovery::minimal,
    };
}

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
        discovery: TransparentAddressDiscovery::minimal,
    })
    .collect();
    let discoveries = vec![
        Variant {
            name: "high/disabled",
            performance: PerformanceLevel::High,
            discovery: TransparentAddressDiscovery::disabled,
        },
        Variant::DEFAULT,
        Variant {
            name: "high/recovery",
            performance: PerformanceLevel::High,
            discovery: TransparentAddressDiscovery::recovery,
        },
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

async fn client(net: &mut MockNet, variant: &Variant) -> LightClient {
    let client = net.client(WALLET_SEED).await;
    client.wallet().write().await.wallet_settings.sync_config = SyncConfig {
        transparent_address_discovery: (variant.discovery)(),
        performance_level: variant.performance,
        ..SyncConfig::default()
    };
    client
}

struct Measurement {
    scenario: &'static str,
    variant: &'static str,
    phase: &'static str,
    run: usize,
    elapsed: Duration,
    result: SyncResult,
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
        })
        .to_string()
    }
}

async fn timed(client: &mut LightClient, rescan: bool) -> (Duration, SyncResult) {
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
                return (start.elapsed(), result.expect("the mock chain syncs"));
            }
        }
    }
}

async fn run_scenario(
    scenario: &Scenario,
    blocks: u32,
    runs: usize,
    wallet: &Addresses,
    foreign: &Addresses,
) -> Vec<Measurement> {
    let mut net = MockNet::launch().await;
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
            let mut client = client(&mut net, variant).await;
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
            });
            clients.push((variant, run, client));
        }
    }

    mine(&net, &scenario.shape, wallet, foreign, blocks / 10).await;
    for (variant, run, client) in &mut clients {
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
        });
    }
    for (variant, run, client) in &mut clients {
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
    for scenario in scenarios() {
        if !filters.is_empty() && !filters.iter().any(|filter| scenario.name.contains(filter)) {
            continue;
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

    println!("\n| scenario | variant | phase | blocks | outputs (s/o/i) | best s | median s |");
    println!("| --- | --- | --- | --- | --- | --- | --- |");
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
        println!(
            "| {scenario} | {variant} | {phase} | {} | {}/{}/{} | {:.2} | {:.2} |",
            sample.result.blocks_scanned,
            sample.result.sapling_outputs_scanned,
            sample.result.orchard_outputs_scanned,
            sample.result.ironwood_outputs_scanned,
            seconds[0],
            seconds[seconds.len() / 2],
        );
    }
}
