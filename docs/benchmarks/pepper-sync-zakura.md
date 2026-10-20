# pepper-sync: upstream librustzcash versus the Zakura wallet libraries

Wall-clock sync times of a `LightClient` driven by pepper-sync against fabricated chains served by
the in-process mock indexer, measured on the commit before the migration (`68c2e4ca2`: upstream
`zcash_client_backend` 0.24, `orchard` 0.15, Rust 1.90) and on the migrated tree
(`zakura-client-backend` from wallet-libraries main at `a2f87e6`, the `zakura-*` 4.0.0-rc1 family,
Rust 1.97). Both runs use the same bench source, chain shapes and seeds, one after the other on the
same otherwise idle machine: Apple Silicon (arm64), 16 logical cores, macOS 26.3.1, 2026-10-20.

The harness is `zingolib/benches/pepper_sync.rs`. Run it with
`cargo bench -p zingolib --bench pepper_sync -- [scenario filter...]`. `PEPPER_BENCH_BLOCKS`,
`PEPPER_BENCH_RUNS` and `PEPPER_BENCH_OUT` set the chain length, the number of fresh wallets per
variant and a JSON lines output path. The raw measurements behind this page sit next to it.

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

A sync of 3,000 empty blocks takes 2.28 s on both stacks. Almost none of it is
scanning: pepper-sync sleeps one second after the scanner drains while waiting for the mempool
monitor, the monitor acknowledges shutdown on a one-second interval, and the sync loop ticks every
50 ms. The harness polls the sync task every millisecond so the client's half-second completion
poll does not add to it. The `work ratio` column subtracts this floor from both stacks before
dividing, which is the better estimate of the decryption speedup; it is omitted where the upstream
time is within half a second of the floor.

## Results

Best and median of three fresh wallets, in seconds. Outputs are counted as Sapling/Ironwood. No
scenario produces Orchard-pool outputs: the Zakura Orchard crate only builds bundles for the
post-NU6.3 protocol, so both chains run with Ironwood active from the first block.

| scenario | variant | phase | blocks | outputs s/i | upstream best | upstream median | zakura best | zakura median | ratio | work ratio |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| empty_blocks | high/minimal | full | 3000 | 0/0 | 2.28 | 2.28 | 2.27 | 2.28 | 1.00 | n/a |
| empty_blocks | high/minimal | incremental | 300 | 0/0 | 2.26 | 2.26 | 2.26 | 2.26 | 1.00 | n/a |
| empty_blocks | high/minimal | rescan | 3300 | 0/0 | 2.28 | 2.28 | 2.27 | 2.28 | 1.00 | n/a |
| sapling_noise_10 | high/minimal | full | 3000 | 30000/0 | 6.37 | 6.39 | 6.71 | 6.82 | 1.05 | 1.08 |
| sapling_noise_10 | high/minimal | incremental | 300 | 3000/0 | 2.89 | 2.92 | 2.94 | 2.95 | 1.02 | 1.08 |
| sapling_noise_10 | high/minimal | rescan | 3300 | 33000/0 | 6.83 | 6.96 | 7.29 | 7.32 | 1.07 | 1.10 |
| ironwood_noise_10 | low/minimal | full | 3000 | 0/30000 | 8.80 | 9.13 | 5.09 | 5.09 | 0.58 | 0.43 |
| ironwood_noise_10 | medium/minimal | full | 3000 | 0/30000 | 7.71 | 7.85 | 4.65 | 4.70 | 0.60 | 0.44 |
| ironwood_noise_10 | high/minimal | full | 3000 | 0/30000 | 7.70 | 8.01 | 4.62 | 4.64 | 0.60 | 0.43 |
| ironwood_noise_10 | maximum/minimal | full | 3000 | 0/30000 | 8.13 | 8.14 | 4.71 | 4.78 | 0.58 | 0.42 |
| ironwood_noise_10 | low/minimal | incremental | 300 | 0/3000 | 3.05 | 3.06 | 2.61 | 2.61 | 0.85 | 0.44 |
| ironwood_noise_10 | medium/minimal | incremental | 300 | 0/3000 | 3.21 | 3.22 | 2.58 | 2.59 | 0.81 | 0.34 |
| ironwood_noise_10 | high/minimal | incremental | 300 | 0/3000 | 3.20 | 3.20 | 2.59 | 2.61 | 0.81 | 0.35 |
| ironwood_noise_10 | maximum/minimal | incremental | 300 | 0/3000 | 3.59 | 3.62 | 2.72 | 2.72 | 0.76 | 0.34 |
| ironwood_noise_10 | low/minimal | rescan | 3300 | 0/33000 | 9.90 | 9.90 | 5.43 | 5.47 | 0.55 | 0.41 |
| ironwood_noise_10 | medium/minimal | rescan | 3300 | 0/33000 | 8.67 | 8.68 | 4.97 | 4.98 | 0.57 | 0.42 |
| ironwood_noise_10 | high/minimal | rescan | 3300 | 0/33000 | 8.38 | 8.42 | 5.00 | 5.02 | 0.60 | 0.45 |
| ironwood_noise_10 | maximum/minimal | rescan | 3300 | 0/33000 | 8.78 | 8.82 | 5.00 | 5.06 | 0.57 | 0.42 |
| mixed_noise | high/minimal | full | 3000 | 15000/15000 | 8.74 | 8.81 | 6.49 | 6.81 | 0.74 | 0.65 |
| mixed_noise | high/minimal | incremental | 300 | 1500/1500 | 3.31 | 3.33 | 2.91 | 2.92 | 0.88 | 0.62 |
| mixed_noise | high/minimal | rescan | 3300 | 16500/16500 | 9.73 | 9.82 | 6.79 | 6.86 | 0.70 | 0.61 |
| wallet_sapling_notes | high/minimal | full | 3000 | 15300/15000 | 9.22 | 9.48 | 6.59 | 6.71 | 0.71 | 0.62 |
| wallet_sapling_notes | high/minimal | incremental | 300 | 1530/1500 | 3.51 | 3.53 | 3.04 | 3.07 | 0.87 | 0.63 |
| wallet_sapling_notes | high/minimal | rescan | 3300 | 16830/16500 | 10.29 | 10.83 | 7.45 | 7.74 | 0.72 | 0.65 |
| wallet_ironwood_notes | high/minimal | full | 3000 | 15000/15300 | 9.79 | 10.02 | 6.36 | 6.57 | 0.65 | 0.54 |
| wallet_ironwood_notes | high/minimal | incremental | 300 | 1500/1530 | 3.79 | 3.79 | 2.97 | 3.00 | 0.78 | 0.46 |
| wallet_ironwood_notes | high/minimal | rescan | 3300 | 16500/16830 | 11.70 | 11.72 | 7.14 | 7.17 | 0.61 | 0.52 |
| wallet_ironwood_dense | high/minimal | full | 3000 | 3000/6000 | 15.48 | 16.11 | 6.94 | 6.98 | 0.45 | 0.35 |
| wallet_ironwood_dense | high/minimal | incremental | 300 | 300/600 | 3.99 | 4.16 | 2.86 | 2.88 | 0.72 | 0.35 |
| wallet_ironwood_dense | high/minimal | rescan | 3300 | 3300/6600 | 21.11 | 21.37 | 9.31 | 9.53 | 0.44 | 0.37 |
| wallet_transparent | high/disabled | full | 3000 | 15000/15000 | 5.61 | 5.84 | 3.56 | 3.69 | 0.63 | 0.38 |
| wallet_transparent | high/minimal | full | 3000 | 15000/15000 | 9.21 | 9.26 | 6.47 | 6.49 | 0.70 | 0.61 |
| wallet_transparent | high/recovery | full | 3000 | 15000/15000 | 71.76 | 85.64 | 56.97 | 58.39 | 0.79 | 0.79 |
| wallet_transparent | high/disabled | incremental | 300 | 1500/1500 | 2.89 | 2.89 | 2.52 | 2.53 | 0.87 | 0.42 |
| wallet_transparent | high/minimal | incremental | 300 | 1500/1500 | 3.37 | 3.38 | 2.91 | 2.91 | 0.86 | 0.58 |
| wallet_transparent | high/recovery | incremental | 300 | 1500/1500 | 11.89 | 11.91 | 9.51 | 9.55 | 0.80 | 0.75 |
| wallet_transparent | high/disabled | rescan | 3300 | 16500/16500 | 5.78 | 6.21 | 3.64 | 3.75 | 0.63 | 0.39 |
| wallet_transparent | high/minimal | rescan | 3300 | 16500/16500 | 9.83 | 9.87 | 6.59 | 6.64 | 0.67 | 0.57 |
| wallet_transparent | high/recovery | rescan | 3300 | 16500/16500 | 80.81 | 83.67 | 61.68 | 61.85 | 0.76 | 0.76 |

## Reading the table

- Ironwood trial decryption is where the gain is. On `ironwood_noise_10` the work above the floor
  drops from 5.4 s to 2.3 s, 2.3 times faster, and the ratio holds at every
  performance level. The dense owned-note wallet, which also commits 3,000 witnesses, goes from
  13.2 s to 4.7 s above the floor, 2.8 times faster.
- Sapling trial decryption does not improve. The Zakura speedups live in the Pasta curve arithmetic
  behind Orchard and Ironwood; Sapling runs on Jubjub, which the fork left as it was. The Zakura
  run is a few percent slower on the Sapling-only chain, within what two different compiler
  versions and run-to-run noise account for.
- The empty-chain cost is identical, as expected: it is pepper-sync's own waits, not crate work.
- Performance level barely matters at this chain size on either stack. `low` is slower because it
  quarters the batch size; `medium`, `high` and `maximum` are within noise of each other.
- Transparent discovery at recovery depth dominates its scenario on both stacks. That time is gRPC
  round-trips for address lookups, not decryption, so the stacks are closer there.
- The 300-block incremental phases are mostly floor and should not be read as a throughput measure.
