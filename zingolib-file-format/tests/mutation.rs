//! Corrupts a valid wallet file one byte, one truncation, and one oversized length prefix at a
//! time. Every truncation must be rejected with an error. A byte flip or a length prefix
//! blown up to its maximum may decode as a different but well-formed wallet, so those must
//! never panic and never hang, and are otherwise free to be accepted or rejected.
//!
//! Covers a fresh wallet per chain (plaintext and encrypted) at every offset, and every
//! `*.dat` vector under `tests/vectors/` at a sample of offsets. The `slow` test samples the
//! vectors densely and is filtered out of the default CI run.

mod support;

use std::path::{Path, PathBuf};

use zingo_common_components::protocol::ActivationHeights;
use zingolib_common::chain::ChainType;

use support::{Coverage, bytes, fast_session, fresh, mutation_sweep};

/// Passphrase used for every encrypted fixture in this file, including vector files whose name
/// marks them as encrypted.
const PASSPHRASE: &str = "conformance";

/// Offsets sampled per vector in the default run, and in the `slow` run.
const VECTOR_SAMPLES: usize = 16;
const VECTOR_SAMPLES_SLOW: usize = 256;

/// `tests/vectors/` is expected to stay small (pinned-release wallet files); this just guards
/// against an unbounded walk if something unexpected lands there.
const MAX_VECTOR_FILES: usize = 10_000;
/// Bounds the recursive walk of `tests/vectors/` so a symlink loop can't spin forever.
const MAX_WALK_DEPTH: usize = 16;

fn chains() -> [ChainType; 3] {
    [
        ChainType::Mainnet,
        ChainType::Testnet,
        ChainType::Regtest(ActivationHeights::default()),
    ]
}

#[test]
fn fresh_wallets_reject_every_mutation() {
    for (index, chain_type) in chains().into_iter().enumerate() {
        let file = fresh(chain_type, index as u8);
        let plain = bytes(&file);
        mutation_sweep(
            &format!("fresh {chain_type} plain"),
            &plain,
            None,
            Coverage::Exhaustive,
        );

        let envelope = fast_session(PASSPHRASE).encrypt(&plain).unwrap();
        mutation_sweep(
            &format!("fresh {chain_type} encrypted"),
            &envelope,
            Some(PASSPHRASE),
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
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/vectors");
    if !root.exists() {
        return;
    }

    let files = wallet_dat_files(&root);
    assert!(
        files.len() <= MAX_VECTOR_FILES,
        "tests/vectors/ holds {} .dat files, more than the expected cap of {MAX_VECTOR_FILES}",
        files.len()
    );

    for path in files {
        let contents =
            std::fs::read(&path).unwrap_or_else(|e| panic!("failed to read {path:?}: {e}"));
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        let passphrase = name.contains("encrypted").then_some(PASSPHRASE);
        mutation_sweep(&path.display().to_string(), &contents, passphrase, coverage);
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
            "tests/vectors/ nests deeper than {MAX_WALK_DEPTH} levels at {dir:?}"
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
