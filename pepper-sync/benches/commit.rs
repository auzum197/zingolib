//! Criterion benchmark of the commit phase (shardtree insert + prune).
//!
//! Run with `cargo bench -p pepper-sync`. Uses a modest batch because Criterion re-runs the
//! routine many times. The full-scale timing is the `commit_full_scale` ignored test in
//! `bench_support`. Each iteration commits a fresh batch into a shard already partially filled by
//! a prior batch, the steady-state ChainTip condition.

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};

use pepper_sync::bench_support::{
    MAXIMUM_LOCATED_TREE_SIZE, commit_batch, prefill_shard, synthetic_orchard_leaves,
    synthetic_sapling_leaves,
};
use pepper_sync::wallet::ShardTrees;

fn commit(c: &mut Criterion) {
    let blocks = 4_000u32;
    let sapling_outputs = 1_000u32;
    let orchard_outputs = 3_000u32;

    let mut group = c.benchmark_group("commit");
    group.sample_size(20);
    group.bench_function("4k_outputs_partial_shard", |b| {
        b.iter_batched(
            || {
                let mut trees = ShardTrees::new();
                let (sapling_pos, orchard_pos) = prefill_shard(
                    &mut trees,
                    blocks,
                    sapling_outputs + orchard_outputs,
                    MAXIMUM_LOCATED_TREE_SIZE,
                );
                let sapling = synthetic_sapling_leaves(blocks, sapling_outputs, &[10, 500]);
                let orchard = synthetic_orchard_leaves(blocks, orchard_outputs, &[20, 600]);
                (trees, sapling, orchard, sapling_pos, orchard_pos)
            },
            |(mut trees, sapling, orchard, sapling_pos, orchard_pos)| {
                std::hint::black_box(commit_batch(
                    &mut trees,
                    sapling,
                    orchard,
                    sapling_pos,
                    orchard_pos,
                    MAXIMUM_LOCATED_TREE_SIZE,
                ))
            },
            BatchSize::SmallInput,
        );
    });
    group.finish();
}

criterion_group!(benches, commit);
criterion_main!(benches);
