# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Deprecated

### Fixed
- A hash discontinuity more than `MAX_REORG_ALLOWANCE` blocks below the chain height the wallet's
  state was formed against clears the wallet with the chain error. The depth was measured from the
  wallet's highest scanned block, so a wallet stopped between the batches of a task attempted a
  truncation there and was cleared with a truncation error instead.

### Added
- `wallet::OutputId`, `keys::KeyId` and `keys::transparent::TransparentAddressId`: unversioned
  `read` and `write`, replacing the field-by-field encoding repeated in every note writer.
- `events` module: push-based sync event stream so consumers subscribe to committed
  events instead of polling `sync_status` under the wallet lock.
  - `events::SyncEvent` - `SessionStarted`, `BatchScanStarted`, `BatchScanCompleted`,
    `BatchCommitStarted`, `RangeScanned`, `TxDiscovered`, `Reorg` and `TipMoved`
    variants. `SessionStarted` carries the note-commitment totals and already-scanned
    baseline needed to compute a progress metric consumer-side. `BatchScanStarted`
    announces the exact output counts of a batch as a worker takes it, for accurate
    in-flight progress. `BatchScanCompleted` and `BatchCommitStarted` mark the batch
    lifecycle transitions (scanning → waiting for the serialized commit stage →
    committing), so consumers can show a batch waiting behind another's commit rather
    than guessing. `RangeScanned` carries a `ScanTiming` breakdown (fetch, decryption,
    tree construction, and commit) measured by the engine, so consumers estimate each
    phase, including the commit, from measured times rather than from commit wall-clock.
  - `events::ScanTiming` - the per-phase wall-clock cost carried by `RangeScanned`, with a
    `total()` helper. Its `commit` field is an `events::CommitTiming` breakdown (checkpoints,
    frontiers, insert_tree, spend_fetch, spend_cpu, cleanup, other) for commit-phase tuning.
    Both re-exported in lib.rs.
  - `events::SequencedSyncEvent` - a `SyncEvent` with its stream position, the type
    delivered to subscribers.
  - `events::SyncEmitter` - non-blocking, lag-tolerant emitter built on
    `tokio::sync::broadcast`. Constructed by the consumer and passed to `sync`.
  - re-exported as `SequencedSyncEvent`/`SyncEmitter`/`SyncEvent` in lib.rs.
- `config::SyncConfig`: added `event_channel_capacity` field (default 512, see
  `config::DEFAULT_EVENT_CHANNEL_CAPACITY`). Serialized version incremented to 2.

### Changed
- `sync::MAX_REORG_ALLOWANCE` is 1000, following zebra's `MAX_BLOCK_REORG_HEIGHT`, which zebra
  5.2.0 raised from 100 and has kept since. A re-org up to 1000 blocks deep is truncated and
  scanned again, where a re-org deeper than 100 blocks cleared the wallet's chain data before.
  The wallet keeps the wallet blocks and, through the new
  `sync::SHARDTREE_CHECKPOINT_ROLLING_WINDOW_SIZE`, the shard tree checkpoints of the last 1000
  blocks, and transparent address discovery searches the 1000 blocks below the last known chain
  height at the start of a sync session. A wallet whose last known chain height is more than 1000
  blocks above the server's chain height returns `SyncError::ChainError`, where the bound was 100.
- Scanning places a shard tree checkpoint only on the blocks the wallet will hold one for: the
  blocks inside the rolling window hanging from the chain height when the scan task was selected,
  and the last block of every batch, so a session stopped after it anchors a spend there. Every
  other block's checkpoint was built, inserted and pruned from each pool's tree for nothing, a
  fixed cost per block per active pool. The heights come from one function,
  `witness::checkpoint_heights`, which the shard tree update also uses to decide the checkpoints
  it adds for blocks that leave a pool's tree unchanged, so the two sides agree. A scan task
  carries the chain height from its selection for this.
- BREAKING: `wallet::traits::SyncShardTrees::update_shard_trees` takes the chain height the scan
  task was selected at in place of the wallet's highest scanned block. A batch's rolling
  checkpoint window hangs from the chain height, so only the batches inside the top window of the
  chain place it, whatever task they belong to. Before, every batch above the highest scanned
  height placed a full window hanging from its own last block, which the next batch's inserts
  pruned.
- wallet readers reject malformed input with `InvalidData` where they could panic or allocate
  without bound: non-canonical nullifiers, rho and rseed values, invalid recipients, values
  above the money range, scan ranges that end before they start or leave gaps, repeated scan
  targets and nullifiers, shard trees with malformed nodes or more checkpoints than the writer
  keeps, and struct versions older than the first release.
- `WalletTransaction::with_outgoing_sapling_notes_for_test` and `with_ironwood_notes_for_test`
  under `test-features`, so a test transaction can carry notes in every pool at once.
- `keys::transparent::TransparentScope`: now defined in `zingolib_common::keys` and re-exported.
- wallet types and `config::SyncConfig` / `config::PerformanceLevel`: inherent `read` and
  `write` replaced by `zingolib_common::serialization::ReadableWriteable` impls. Reading now
  rejects layout versions newer than the crate knows, and `write` takes `&self` for
  `SyncState` and `ShardTrees`.
- `sync::sync` fn: added `events: SyncEmitter` parameter.
- `sync::sync_status` fn: reduced to a reconcile getter returning facts only
  (scan range coverage and cumulative scanned counts). Derived metrics, including
  any progress percentage, are now computed consumer-side from the event stream.
- `sync::SyncStatus`: `Display` impl now summarises raw counts.

### Removed
- Struct layouts older than the ones the first pendrake-watch release wrote: scan target and
  output id fallbacks, `SyncState` scan priorities before layout 3, and the `SyncConfig`
  defaults for layouts 0 and 1. The Ironwood upgrades (`SyncState` 4, `NullifierMap` 2,
  `TreeBounds` 1, `WalletTransaction` 1, `ShardTrees` 1) stay readable from their predecessors.
- `sync::SyncStatus`: removed `percentage_session_blocks_scanned`,
  `percentage_total_blocks_scanned`, `percentage_session_outputs_scanned`,
  `percentage_total_outputs_scanned`, `session_blocks_scanned`,
  `session_sapling_outputs_scanned` and `session_orchard_outputs_scanned` fields,
  along with their keys in the `json::JsonValue` conversion. The 99%-until-verified
  clamp is gone with them; consumers choose their own model (see
  `events::SyncEvent::SessionStarted`).
- `sync::SyncResult`: removed `percentage_total_outputs_scanned` field and its
  `Display`/`json::JsonValue` output.

## [0.5.0] - 2026-06-10

### Added
- `error::SyncRecoveryObservables` enum with variants `MaybeRecoverableServer`,
  `ServerUnavailable`, and `Abort` — classifies sync errors for consumer retry logic.
- `error::SyncError::is_retryable()` — returns `true` for transient errors
  (server timeouts, connection drops, mempool failures).
- `error::SyncError::recovery_recommendation()` — maps any sync error to a
  `SyncRecoveryObservables` without callers needing to match on error internals.
- `error::ServerError::is_retryable()` — distinguishes transport failures from
  invalid server data.
- `error::ServerError::recovery_recommendation()` — server-level recovery classification.

### Changed
- `wallet::TransparentCoin`: serialized version incremented to 1 to serialize output indexes as u32
- `wallet::WalletNote`: serialized version incremented to 2 to serialize output indexes as u32
- `wallet::OutgoingNote`: serialized version incremented to 1 to serialize output indexes as u32
- `wallet::OutputId`:
  - `output_index` field is now u32.
  - `new` constructor `output_index` parameter is now u32.
  - `output_index` method's return type is now u32.

## [0.4.0] - 2026-06-05

### Added
`wallet::WalletTransaction`: added `total_external_outgoing_note_value` method

## [0.3.0]

### Changed

- `sync::sync` fn: `client` parameter now takes a `CompactTxStreamerClient<tonic::Channel>`

## [0.2.0] - 2026-02-26

### Added
- `wallet::WalletTransaction::update_status`
- `wallet::WalletTransaction::new_for_test`
- `sync::set_transactions_failed` - also re-exported in lib.rs

### Changed
- `error::SyncError`:
  - added `BirthdayBelowSapling` variant which is returned when `sync` is called with wallet birthday below sapling activation height.
  - `ChainError` variant now includes the wallet height and chain height.
- `error::ScanError`:
  - `InvalidMemoBytes` variant now uses `zcash_protocol::memo::Error` instead of deprecated `zcash_primitives::memo::Error` type.
- `keys::KeyID` now uses `zip32::AccountId` directly instead of `zcash_primitives` re-export.
- `keys::ScanningKeyOps` trait now uses `zip32::AccountId` directly instead of `zcash_primitives` re-export.
- `keys::TransparentAddressId` now uses `zip32::AccountId` directly instead of `zcash_primitives` re-export.
- `sync::ScanPriority`:
  - added `RefetchingNullifiers` variant.
- `wallet::SyncState`:
  - incremented to serialized version 3 to account for changes to `ScanPriority`
  - `wallet_height` method renamed to `last_known_chain_height`.
- `wallet::NoteInterface` trait: added `refetch_nullifier_ranges` method.
- `wallet::SaplingNote`:
  - implemented `refetch_nullifier_ranges` method.
  - updated serialization to account for new `WalletNote` field.
- `wallet::OrchardNote`:
  - implemented `refetch_nullifier_ranges` method.
  - updated serialization to account for new `WalletNote` field.
- `wallet::WalletNote`:
  - incremented to serialized version 1 to account for changes to `WalletNote` struct.

### Removed

## [0.1.0] - 2026-01-09
