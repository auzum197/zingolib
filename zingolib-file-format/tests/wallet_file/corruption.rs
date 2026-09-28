//! Corrupts a valid wallet file and reads it back. Every truncation must be rejected with an
//! error. A byte flip or a length prefix blown up to its maximum may decode as a different but
//! well-formed wallet, so those must never panic and never hang, and are otherwise free to be
//! accepted or rejected.
//!
//! The corpus is a fresh wallet per chain, plaintext and encrypted, plus every `*.dat` vector
//! under `tests/wallet_file/data/`. The default run sweeps single mutations: every offset of a
//! fresh wallet, a sample of offsets of each vector. The `slow` runs sample the vectors densely
//! and chain several mutations of every kind from a fixed seed, so a failure replays exactly
//! and shrinks to the shortest chain that still triggers it.

use std::path::{Path, PathBuf};

use proptest::prelude::*;
use proptest::test_runner::{Config, RngAlgorithm, TestCaseError, TestRng, TestRunner};

use zingo_common_components::protocol::ActivationHeights;
use zingolib_common::chain::ChainType;

use super::support::{
    COMPACT_SIZE_MAX_ENCODINGS, Coverage, bytes, fast_session, fresh, mutation_sweep, read_survives,
};

/// Passphrase used for every encrypted fixture in this file, including vector files whose name
/// marks them as encrypted.
const PASSPHRASE: &str = "conformance";

/// Offsets sampled per vector in the default run, and in the `slow` run.
const VECTOR_SAMPLES: usize = 16;
const VECTOR_SAMPLES_SLOW: usize = 256;

/// Seed of the mutation-chain run. `WALLET_FILE_FUZZ_SEED` overrides it, to replay a failure
/// from another run or to explore a different region.
const FUZZ_SEED: u64 = 0x2026_0926;
const FUZZ_CASES: u32 = 4096;
const MAX_CHAIN_LENGTH: usize = 6;
const MAX_SPLICE_LENGTH: usize = 16;

/// `tests/wallet_file/data/` is expected to stay small (pinned-release wallet files); this just guards
/// against an unbounded walk if something unexpected lands there.
const MAX_VECTOR_FILES: usize = 10_000;
/// Bounds the recursive walk of `tests/wallet_file/data/` so a symlink loop can't spin forever.
const MAX_WALK_DEPTH: usize = 16;

fn chains() -> [ChainType; 3] {
    [
        ChainType::Mainnet,
        ChainType::Testnet,
        ChainType::Regtest(ActivationHeights::default()),
    ]
}

struct Input {
    label: String,
    bytes: Vec<u8>,
    passphrase: Option<&'static str>,
}

fn fresh_inputs() -> Vec<Input> {
    chains()
        .into_iter()
        .enumerate()
        .flat_map(|(index, chain_type)| {
            let plain = bytes(&fresh(chain_type, index as u8));
            let envelope = fast_session(PASSPHRASE).encrypt(&plain).unwrap();
            [
                Input {
                    label: format!("fresh {chain_type} plain"),
                    bytes: plain,
                    passphrase: None,
                },
                Input {
                    label: format!("fresh {chain_type} encrypted"),
                    bytes: envelope,
                    passphrase: Some(PASSPHRASE),
                },
            ]
        })
        .collect()
}

fn vector_inputs() -> Vec<Input> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/wallet_file/data");
    if !root.exists() {
        return Vec::new();
    }

    let files = wallet_dat_files(&root);
    assert!(
        files.len() <= MAX_VECTOR_FILES,
        "tests/wallet_file/data/ holds {} .dat files, more than the expected cap of {MAX_VECTOR_FILES}",
        files.len()
    );

    files
        .into_iter()
        .map(|path| {
            let contents =
                std::fs::read(&path).unwrap_or_else(|e| panic!("failed to read {path:?}: {e}"));
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default();
            Input {
                passphrase: name.contains("encrypted").then_some(PASSPHRASE),
                label: path.display().to_string(),
                bytes: contents,
            }
        })
        .collect()
}

#[test]
fn fresh_wallets_reject_every_mutation() {
    for input in fresh_inputs() {
        mutation_sweep(
            &input.label,
            &input.bytes,
            input.passphrase,
            Coverage::Exhaustive,
        );
    }
}

#[test]
fn vector_files_survive_sampled_mutations() {
    sweep_vectors(Coverage::Sampled(VECTOR_SAMPLES));
}

#[test]
fn vector_files_survive_dense_mutations_slow() {
    sweep_vectors(Coverage::Sampled(VECTOR_SAMPLES_SLOW));
}

fn sweep_vectors(coverage: Coverage) {
    for input in vector_inputs() {
        mutation_sweep(&input.label, &input.bytes, input.passphrase, coverage);
    }
}

/// One edit to a byte string. Offsets are taken modulo the current length, so a chain stays
/// valid as earlier edits grow or shrink the input, and a shrunk offset is still an offset.
#[derive(Debug, Clone)]
enum Mutation {
    Flip { offset: u32, mask: u8 },
    Overwrite { offset: u32, bytes: Vec<u8> },
    Insert { offset: u32, bytes: Vec<u8> },
    Delete { offset: u32, len: u8 },
    Truncate { offset: u32 },
    MaxLength { offset: u32, encoding: u8 },
}

impl Mutation {
    fn apply(&self, bytes: &mut Vec<u8>) {
        let len = bytes.len();
        if len == 0 {
            return;
        }
        let at = |offset: u32| offset as usize % len;
        match self {
            Mutation::Flip { offset, mask } => bytes[at(*offset)] ^= mask,
            Mutation::Overwrite {
                offset,
                bytes: patch,
            } => overwrite(bytes, at(*offset), patch),
            Mutation::Insert {
                offset,
                bytes: patch,
            } => {
                let start = at(*offset);
                bytes.splice(start..start, patch.iter().copied());
            }
            Mutation::Delete { offset, len: count } => {
                let start = at(*offset);
                bytes.drain(start..(start + usize::from(*count)).min(len));
            }
            Mutation::Truncate { offset } => bytes.truncate(at(*offset)),
            Mutation::MaxLength { offset, encoding } => {
                let patch = COMPACT_SIZE_MAX_ENCODINGS
                    [usize::from(*encoding) % COMPACT_SIZE_MAX_ENCODINGS.len()];
                overwrite(bytes, at(*offset), patch);
            }
        }
    }
}

fn overwrite(bytes: &mut [u8], start: usize, patch: &[u8]) {
    let end = (start + patch.len()).min(bytes.len());
    bytes[start..end].copy_from_slice(&patch[..end - start]);
}

fn mutation_strategy() -> impl Strategy<Value = Mutation> {
    let splice = || prop::collection::vec(any::<u8>(), 1..=MAX_SPLICE_LENGTH);
    prop_oneof![
        (any::<u32>(), 1u8..).prop_map(|(offset, mask)| Mutation::Flip { offset, mask }),
        (any::<u32>(), splice()).prop_map(|(offset, bytes)| Mutation::Overwrite { offset, bytes }),
        (any::<u32>(), splice()).prop_map(|(offset, bytes)| Mutation::Insert { offset, bytes }),
        (any::<u32>(), 1u8..).prop_map(|(offset, len)| Mutation::Delete { offset, len }),
        any::<u32>().prop_map(|offset| Mutation::Truncate { offset }),
        (any::<u32>(), any::<u8>())
            .prop_map(|(offset, encoding)| Mutation::MaxLength { offset, encoding }),
    ]
}

fn fuzz_seed() -> u64 {
    std::env::var("WALLET_FILE_FUZZ_SEED")
        .ok()
        .map(|seed| {
            seed.parse()
                .unwrap_or_else(|e| panic!("WALLET_FILE_FUZZ_SEED {seed:?} is not a u64: {e}"))
        })
        .unwrap_or(FUZZ_SEED)
}

#[test]
fn inputs_survive_mutation_chains_slow() {
    let inputs = fresh_inputs()
        .into_iter()
        .chain(vector_inputs())
        .collect::<Vec<_>>();
    let seed = fuzz_seed();
    let mut seed_bytes = [0u8; 32];
    seed_bytes[..8].copy_from_slice(&seed.to_le_bytes());
    let config = Config {
        cases: FUZZ_CASES,
        source_file: Some(file!()),
        ..Config::default()
    };
    let mut runner = TestRunner::new_with_rng(
        config,
        TestRng::from_seed(RngAlgorithm::ChaCha, &seed_bytes),
    );
    let chains = (
        0..inputs.len(),
        prop::collection::vec(mutation_strategy(), 1..=MAX_CHAIN_LENGTH),
    );
    let outcome = runner.run(&chains, |(index, chain)| {
        let input = &inputs[index];
        let mut mutated = input.bytes.clone();
        for mutation in &chain {
            mutation.apply(&mut mutated);
        }
        read_survives(&mutated, input.passphrase)
            .map_err(|failure| TestCaseError::fail(format!("{}: {failure}", input.label)))
    });
    if let Err(failure) = outcome {
        panic!("mutation chain from seed {seed:#x}: {failure}");
    }
}

/// Every `*.dat` file under `root`, found by an explicit (bounded) stack-based walk rather than
/// recursion so a deeply nested directory can't overflow the stack.
fn wallet_dat_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];

    while let Some((dir, depth)) = stack.pop() {
        assert!(
            depth <= MAX_WALK_DEPTH,
            "tests/wallet_file/data/ nests deeper than {MAX_WALK_DEPTH} levels at {dir:?}"
        );
        let entries =
            std::fs::read_dir(&dir).unwrap_or_else(|e| panic!("failed to read {dir:?}: {e}"));
        for entry in entries {
            let path = entry
                .unwrap_or_else(|e| panic!("failed to read entry in {dir:?}: {e}"))
                .path();
            if path.is_dir() {
                stack.push((path, depth + 1));
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("dat") {
                files.push(path);
            }
        }
    }

    files
}
