# pepper-sync: upstream librustzcash versus the Zakura wallet libraries

Wall-clock sync times of a `LightClient` driven by pepper-sync against fabricated chains served by
the in-process mock indexer, measured on the commit before the migration (`68c2e4ca2`, upstream
`zcash_client_backend` 0.24, `orchard` 0.15, Rust 1.90) and on the migrated tree
(`zakura-client-backend` from wallet-libraries main at `a2f87e6`, the `zakura-*` 4.0.0-rc1 family,
Rust 1.97). Both runs use the same bench source, chain shapes and seeds, sequentially on the same
machine: Apple Silicon (arm64), 16 logical cores, macOS 26.3.1.

The harness is `zingolib/benches/pepper_sync.rs`. Run it with
`cargo bench -p zingolib --bench pepper_sync -- [scenario filter...]`. `PEPPER_BENCH_BLOCKS`,
`PEPPER_BENCH_RUNS` and `PEPPER_BENCH_OUT` set the chain length, the number of fresh wallets per
variant and a JSON lines output path.

## What is measured

Each scenario builds a 3,000-block chain, syncs three fresh wallets from birthday 1 (`full`), mines
300 more blocks of the same shape and syncs them again (`incremental`), then rescans from scratch
(`rescan`). The time covers compact block fetching over local gRPC, trial decryption, note
commitment tree commits and witness updates, nullifier and outpoint mapping, and transparent address
discovery. Chain generation is excluded. Shielded outputs carry zeroed proofs and signatures, so no
proving or proof verification is involved; the stacks differ in note decryption and the field
arithmetic behind it.

Noise outputs pay a foreign wallet and cost one failed trial decryption each. Wallet outputs pay the
benchmark wallet and also cost note recording, witness tracking and spend bookkeeping.

| scenario | per block | isolates |
| --- | --- | --- |
| `empty_blocks` | a transparent coinbase to a foreign address | the fixed per-block cost |
| `sapling_noise_10` | 10 foreign Sapling outputs | Sapling trial decryption |
| `ironwood_noise_10` | 10 foreign Ironwood outputs | Ironwood trial decryption, at every performance level |
| `mixed_noise` | 5 Sapling + 5 Ironwood foreign outputs + coinbase | a mainnet-like mix |
| `wallet_sapling_notes` | mixed noise + a Sapling note to the wallet every 10 blocks | 300 owned Sapling notes |
| `wallet_ironwood_notes` | mixed noise + an Ironwood note to the wallet every 10 blocks | 300 owned Ironwood notes |
| `wallet_ironwood_dense` | 1 + 1 noise + an Ironwood note to the wallet every block | 3,000 owned notes |
| `wallet_transparent` | mixed noise + the coinbase pays the wallet every 10 blocks | transparent discovery at disabled, minimal and recovery depth |

The variant names are `performance level / transparent address discovery`. `high/minimal` is the
wallet's default configuration.

## Floor

A sync of 3,000 empty blocks takes 2.27 s on both stacks. Almost none of it is
scanning: pepper-sync sleeps one second after the scanner drains while waiting for the mempool
monitor, the monitor acknowledges shutdown on a one-second interval, and the sync loop ticks every
50 ms. The harness polls the sync task every millisecond so the client's half-second completion
poll does not add to it. The `work ratio` column below subtracts this floor from both stacks before
dividing, which is the better estimate of the decryption speedup; it is omitted where the upstream
time is within half a second of the floor.

## Results

Best and median of three fresh wallets, in seconds. Outputs are counted as Sapling/Ironwood; no
scenario produces Orchard-pool outputs because the Zakura Orchard crate only builds bundles for the
post-NU6.3 protocol, so both chains run with Ironwood active from the first block.

| scenario | variant | phase | blocks | outputs s/i | upstream best | upstream median | zakura best | zakura median | ratio | work ratio |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| empty_blocks | high/minimal | full | 3000 | 0/0 | 2.27 | 2.27 | 2.27 | 2.27 | 1.00 | n/a |
| empty_blocks | high/minimal | incremental | 300 | 0/0 | 2.26 | 2.26 | 2.26 | 2.26 | 1.00 | n/a |
| empty_blocks | high/minimal | rescan | 3300 | 0/0 | 2.27 | 2.27 | 2.27 | 2.27 | 1.00 | n/a |
| sapling_noise_10 | high/minimal | full | 3000 | 30000/0 | 6.56 | 6.69 | 6.64 | 6.80 | 1.01 | 1.02 |
| sapling_noise_10 | high/minimal | incremental | 300 | 3000/0 | 2.91 | 2.91 | 2.93 | 2.94 | 1.01 | 1.03 |
| sapling_noise_10 | high/minimal | rescan | 3300 | 33000/0 | 7.06 | 7.62 | 7.14 | 7.18 | 1.01 | 1.02 |
| ironwood_noise_10 | low/minimal | full | 3000 | 0/30000 | 9.38 | 9.42 | 4.98 | 5.03 | 0.53 | 0.38 |
| ironwood_noise_10 | medium/minimal | full | 3000 | 0/30000 | 8.05 | 8.18 | 4.64 | 4.65 | 0.58 | 0.41 |
| ironwood_noise_10 | high/minimal | full | 3000 | 0/30000 | 7.86 | 7.95 | 4.59 | 4.62 | 0.58 | 0.42 |
| ironwood_noise_10 | maximum/minimal | full | 3000 | 0/30000 | 8.20 | 8.28 | 4.73 | 4.77 | 0.58 | 0.41 |
| ironwood_noise_10 | low/minimal | incremental | 300 | 0/3000 | 3.19 | 3.20 | 2.60 | 2.61 | 0.81 | 0.36 |
| ironwood_noise_10 | medium/minimal | incremental | 300 | 0/3000 | 3.18 | 3.19 | 2.59 | 2.59 | 0.81 | 0.36 |
| ironwood_noise_10 | high/minimal | incremental | 300 | 0/3000 | 3.18 | 3.18 | 2.57 | 2.57 | 0.81 | 0.34 |
| ironwood_noise_10 | maximum/minimal | incremental | 300 | 0/3000 | 3.57 | 3.58 | 2.66 | 2.67 | 0.75 | 0.31 |
| ironwood_noise_10 | low/minimal | rescan | 3300 | 0/33000 | 10.04 | 10.12 | 5.34 | 5.35 | 0.53 | 0.40 |
| ironwood_noise_10 | medium/minimal | rescan | 3300 | 0/33000 | 8.64 | 8.64 | 4.89 | 4.91 | 0.57 | 0.41 |
| ironwood_noise_10 | high/minimal | rescan | 3300 | 0/33000 | 8.49 | 8.55 | 4.90 | 4.93 | 0.58 | 0.42 |
| ironwood_noise_10 | maximum/minimal | rescan | 3300 | 0/33000 | 8.70 | 8.78 | 4.98 | 4.99 | 0.57 | 0.42 |
| mixed_noise | high/minimal | full | 3000 | 15000/15000 | 8.74 | 8.83 | 6.22 | 6.23 | 0.71 | 0.61 |
| mixed_noise | high/minimal | incremental | 300 | 1500/1500 | 3.28 | 3.31 | 2.88 | 2.88 | 0.88 | 0.60 |
| mixed_noise | high/minimal | rescan | 3300 | 16500/16500 | 9.51 | 9.80 | 6.67 | 6.74 | 0.70 | 0.61 |
| wallet_sapling_notes | high/minimal | full | 3000 | 15300/15000 | 9.09 | 9.18 | 6.76 | 6.81 | 0.74 | 0.66 |
| wallet_sapling_notes | high/minimal | incremental | 300 | 1530/1500 | 3.47 | 3.49 | 3.03 | 3.04 | 0.88 | 0.64 |
| wallet_sapling_notes | high/minimal | rescan | 3300 | 16830/16500 | 10.10 | 10.47 | 7.22 | 7.58 | 0.72 | 0.63 |
| wallet_ironwood_notes | high/minimal | full | 3000 | 15000/15300 | 9.33 | 9.44 | 6.34 | 6.36 | 0.68 | 0.58 |
| wallet_ironwood_notes | high/minimal | incremental | 300 | 1500/1530 | 3.71 | 3.71 | 2.95 | 2.96 | 0.80 | 0.48 |
| wallet_ironwood_notes | high/minimal | rescan | 3300 | 16500/16830 | 11.38 | 11.42 | 6.93 | 7.05 | 0.61 | 0.51 |
| wallet_ironwood_dense | high/minimal | full | 3000 | 3000/6000 | 13.32 | 13.52 | 6.94 | 7.05 | 0.52 | 0.42 |
| wallet_ironwood_dense | high/minimal | incremental | 300 | 300/600 | 3.88 | 3.91 | 2.85 | 2.87 | 0.73 | 0.36 |
| wallet_ironwood_dense | high/minimal | rescan | 3300 | 3300/6600 | 18.51 | 18.70 | 9.51 | 9.54 | 0.51 | 0.45 |
| wallet_transparent | high/disabled | full | 3000 | 15000/15000 | 5.62 | 5.70 | 3.55 | 3.72 | 0.63 | 0.38 |
| wallet_transparent | high/minimal | full | 3000 | 15000/15000 | 9.04 | 9.27 | 6.25 | 6.27 | 0.69 | 0.59 |
| wallet_transparent | high/recovery | full | 3000 | 15000/15000 | 71.36 | 72.85 | 57.95 | 58.07 | 0.81 | 0.81 |
| wallet_transparent | high/disabled | incremental | 300 | 1500/1500 | 2.87 | 2.87 | 2.51 | 2.52 | 0.87 | 0.41 |
| wallet_transparent | high/minimal | incremental | 300 | 1500/1500 | 3.33 | 3.34 | 2.88 | 2.88 | 0.86 | 0.58 |
| wallet_transparent | high/recovery | incremental | 300 | 1500/1500 | 11.53 | 11.62 | 10.21 | 10.29 | 0.89 | 0.86 |
| wallet_transparent | high/disabled | rescan | 3300 | 16500/16500 | 6.11 | 6.16 | 3.80 | 3.80 | 0.62 | 0.40 |
| wallet_transparent | high/minimal | rescan | 3300 | 16500/16500 | 9.36 | 9.54 | 7.19 | 7.19 | 0.77 | 0.69 |
| wallet_transparent | high/recovery | rescan | 3300 | 16500/16500 | 75.62 | 76.38 | 65.20 | 65.59 | 0.86 | 0.86 |

## Reading the table

- Ironwood trial decryption is where the gain is. On `ironwood_noise_10` the work above the floor
  drops from about 5.6 s to 2.3 s, roughly 2.4 times faster, and the ratio holds across all four
  performance levels. The dense owned-note wallet, which also commits 3,000 witnesses, shows the
  widest gap.
- Sapling trial decryption is unchanged. The Zakura speedups live in the Pasta curve arithmetic
  behind Orchard and Ironwood; Sapling runs on Jubjub, which the fork did not alter.
- The empty-chain cost is identical, as expected: it is pepper-sync's own waits, not crate work.
- Performance level barely matters at this chain size on either stack. `low` is slower because it
  quarters the batch size; `medium`, `high` and `maximum` are within noise of each other.
- Transparent discovery at recovery depth dominates its scenario on both stacks. That time is gRPC
  round-trips for address lookups, not decryption, so the stacks are closer there.
- The 300-block incremental phases are mostly floor and should not be read as a throughput measure.
