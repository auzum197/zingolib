//! Synthetic workloads for measuring the scan and commit paths offline.
//!
//! Gated behind the `test-features` feature so the `benches/` Criterion targets (separate
//! compilation units) can use it, and available to in-crate tests under `cfg(test)`. The commit
//! driver calls the same [`crate::wallet::traits::insert_located_trees`] the engine uses, so a
//! measured workload exercises the real code.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use incrementalmerkletree::{Marking, Position, Retention};
use orchard::tree::MerkleHashOrchard;
use sapling_crypto::Node;
use zcash_protocol::consensus::BlockHeight;

use crate::wallet::ShardTrees;
use crate::wallet::traits::insert_located_trees;
use crate::witness::build_located_trees;

/// The located-tree chunk size the engine uses at `PerformanceLevel::Maximum`
/// (`max_batch_outputs / 8` = `2^15 / 8`).
pub const MAXIMUM_LOCATED_TREE_SIZE: usize = 4096;

/// Wall-clock split of a synthetic commit, mirroring the live `ScanTiming` `tree`/`commit`
/// breakdown.
#[derive(Clone, Copy, Debug, Default)]
pub struct CommitTiming {
    /// Time spent in [`build_located_trees`] (the `tree` phase live).
    pub tree_build: Duration,
    /// Time spent merging the located trees and pruning (the `insert` sub-phase live).
    pub insert: Duration,
}

/// Builds synthetic sapling leaves and retentions matching what the real scanner produces:
/// `outputs` note commitments spread across `blocks` blocks, a `Retention::Checkpoint` on each
/// block's last leaf (as `set_checkpoint_retentions` does), and `Retention::Marked` on the given
/// output positions. Leaf values are distinct, like real commitments. They were measured not to
/// affect insert and prune cost, but are kept distinct so the workload stays representative.
#[must_use]
pub fn synthetic_sapling_leaves(
    blocks: u32,
    outputs: u32,
    marked: &[u32],
) -> Vec<(Node, Retention<BlockHeight>)> {
    synthetic_leaves(blocks, outputs, marked, |position| {
        Node::from_bytes(distinct_field_bytes(position))
            .into_option()
            .expect("small counter is a canonical field element")
    })
}

/// The orchard counterpart of [`synthetic_sapling_leaves`].
#[must_use]
pub fn synthetic_orchard_leaves(
    blocks: u32,
    outputs: u32,
    marked: &[u32],
) -> Vec<(MerkleHashOrchard, Retention<BlockHeight>)> {
    synthetic_leaves(blocks, outputs, marked, |position| {
        MerkleHashOrchard::from_bytes(&distinct_field_bytes(position))
            .into_option()
            .expect("small counter is a canonical field element")
    })
}

/// A distinct, canonical field-element encoding for a leaf: the position as little-endian bytes.
/// Real note commitments are all distinct, which (unlike a constant value) is representative of
/// the work the commitment tree does.
fn distinct_field_bytes(position: u32) -> [u8; 32] {
    let mut bytes = [0u8; 32];
    bytes[..4].copy_from_slice(&position.to_le_bytes());
    bytes
}

fn synthetic_leaves<L>(
    blocks: u32,
    outputs: u32,
    marked: &[u32],
    make_leaf: impl Fn(u32) -> L,
) -> Vec<(L, Retention<BlockHeight>)> {
    let marked: HashSet<u32> = marked.iter().copied().collect();
    let blocks = blocks.max(1);
    let mut leaves = Vec::with_capacity(outputs as usize);
    let mut position = 0u32;
    for block in 1..=blocks {
        // spread `outputs` across `blocks` as evenly as possible
        let count = outputs / blocks + u32::from(block <= outputs % blocks);
        let block_start = leaves.len();
        for _ in 0..count {
            let retention = if marked.contains(&position) {
                Retention::Marked
            } else {
                Retention::Ephemeral
            };
            leaves.push((make_leaf(position), retention));
            position += 1;
        }
        // mirror `set_checkpoint_retentions`: the block's last leaf becomes a checkpoint
        if leaves.len() > block_start {
            let (_, retention) = leaves.last_mut().expect("block is non-empty");
            *retention = match retention {
                Retention::Marked => Retention::Checkpoint {
                    id: BlockHeight::from_u32(block),
                    marking: Marking::Marked,
                },
                _ => Retention::Checkpoint {
                    id: BlockHeight::from_u32(block),
                    marking: Marking::None,
                },
            };
        }
    }
    leaves
}

/// Commits a synthetic batch into both pools of `shard_trees`, returning the tree-build and
/// insert/prune timings split out (matching the live breakdown). `sapling_position` /
/// `orchard_position` are the number of leaves already committed to each pool. Excludes the
/// Phase-1 `add_checkpoint` loop, which is network-bound and was measured at under 0.2s live.
pub fn commit_batch(
    shard_trees: &mut ShardTrees,
    sapling_leaves: Vec<(Node, Retention<BlockHeight>)>,
    orchard_leaves: Vec<(MerkleHashOrchard, Retention<BlockHeight>)>,
    sapling_position: u64,
    orchard_position: u64,
    located_tree_size: usize,
) -> CommitTiming {
    let build_started = Instant::now();
    let sapling_located = build_located_trees(
        Position::from(sapling_position),
        sapling_leaves,
        located_tree_size,
    );
    let orchard_located = build_located_trees(
        Position::from(orchard_position),
        orchard_leaves,
        located_tree_size,
    );
    let tree_build = build_started.elapsed();

    let insert_started = Instant::now();
    insert_located_trees(&mut shard_trees.sapling, sapling_located).expect("sapling insert");
    insert_located_trees(&mut shard_trees.orchard, orchard_located).expect("orchard insert");
    let insert = insert_started.elapsed();

    CommitTiming { tree_build, insert }
}

/// Commits a marks-free batch to advance both pools to a mid-shard position, reproducing the
/// growing-shard condition before a timed batch. Returns the per-pool leaf count committed, to be
/// passed as the next batch's positions. Assumes a freshly created [`ShardTrees`].
pub fn prefill_shard(
    shard_trees: &mut ShardTrees,
    blocks: u32,
    outputs: u32,
    located_tree_size: usize,
) -> (u64, u64) {
    let sapling = synthetic_sapling_leaves(blocks, outputs, &[]);
    let orchard = synthetic_orchard_leaves(blocks, outputs, &[]);
    let positions = (sapling.len() as u64, orchard.len() as u64);
    commit_batch(shard_trees, sapling, orchard, 0, 0, located_tree_size);
    positions
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Times a real-sized ChainTip commit in isolation. Ignored by default. Run with
    /// `cargo test --release -p pepper-sync -- --ignored --nocapture` and read the breakdown.
    ///
    /// It commits a ~32k-output batch into a shard already partially filled by a prior batch (the
    /// steady-state ChainTip condition). The measured intrinsic insert is about 6s, and a first
    /// batch into an empty shard is about 2s. The live commit is about 51s. Nothing tried here
    /// reproduces that 8x difference: output count, distinct versus constant leaf values, mark
    /// density, tree depth over 40 batches, shard-boundary crossings, high tree positions, and
    /// concurrent CPU or rayon load all leave the insert near 6s. Closing it needs located trees
    /// captured from a live sync and replayed through this same path. Until then this measures the
    /// intrinsic cost and is the fast offline tool for comparing commit changes.
    #[test]
    #[ignore = "intrinsic commit timing, run with --release --ignored --nocapture"]
    fn commit_full_scale() {
        let mut trees = ShardTrees::new();
        // a prior batch leaves the current shard partially filled, as at steady-state ChainTip
        let (sapling_pos, orchard_pos) =
            prefill_shard(&mut trees, 27_234, 32_000, MAXIMUM_LOCATED_TREE_SIZE);
        let sapling = synthetic_sapling_leaves(27_234, 8_000, &[100, 5_000]);
        let orchard = synthetic_orchard_leaves(27_234, 24_000, &[200, 6_000]);
        let timing = commit_batch(
            &mut trees,
            sapling,
            orchard,
            sapling_pos,
            orchard_pos,
            MAXIMUM_LOCATED_TREE_SIZE,
        );
        println!(
            "commit (32k outputs): tree_build={:?} insert={:?} total={:?}",
            timing.tree_build,
            timing.insert,
            timing.tree_build + timing.insert,
        );
    }

    /// Committing a batch with marked notes produces a tree with a valid root and a witness for
    /// every marked note. A small batch (under `MAX_REORG_ALLOWANCE` blocks) so nothing prunes and
    /// all marks stay witnessable. This is the regression guard for future commit optimizations.
    #[test]
    fn commit_produces_valid_tree() {
        let marks = [10u32, 50, 120];
        let mut trees = ShardTrees::new();
        let sapling = synthetic_sapling_leaves(50, 200, &marks);
        let orchard = synthetic_orchard_leaves(50, 200, &marks);
        commit_batch(
            &mut trees,
            sapling,
            orchard,
            0,
            0,
            MAXIMUM_LOCATED_TREE_SIZE,
        );

        assert!(
            trees
                .sapling
                .root_at_checkpoint_depth(Some(0))
                .expect("root")
                .is_some(),
            "sapling tree should have a root"
        );
        for mark in marks {
            assert!(
                trees
                    .sapling
                    .witness_at_checkpoint_depth(Position::from(u64::from(mark)), 0)
                    .expect("witness query")
                    .is_some(),
                "marked sapling position {mark} should be witnessable"
            );
            assert!(
                trees
                    .orchard
                    .witness_at_checkpoint_depth(Position::from(u64::from(mark)), 0)
                    .expect("witness query")
                    .is_some(),
                "marked orchard position {mark} should be witnessable"
            );
        }
    }

    /// Committing the same batch twice yields the same root.
    #[test]
    fn commit_is_deterministic() {
        let marks = [3u32, 17, 99];
        let root = |()| {
            let mut trees = ShardTrees::new();
            let sapling = synthetic_sapling_leaves(60, 250, &marks);
            let orchard = synthetic_orchard_leaves(60, 250, &marks);
            commit_batch(
                &mut trees,
                sapling,
                orchard,
                0,
                0,
                MAXIMUM_LOCATED_TREE_SIZE,
            );
            (
                trees
                    .sapling
                    .root_at_checkpoint_depth(Some(0))
                    .expect("root"),
                trees
                    .orchard
                    .root_at_checkpoint_depth(Some(0))
                    .expect("root"),
            )
        };
        assert_eq!(root(()), root(()), "commit should be deterministic");
    }
}
