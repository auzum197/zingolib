# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `protocol`: `ActivationHeights`, `BlockHeight`, `TxId` and `NetworkType`, copied from
  `zingo_common_components` 0.4.0. The crate no longer depends on it.
- `wallet`: the wallet state a sync engine produces and the wallet file stores, moved from
  `pepper_sync::wallet`: `ScanTarget`, `TreeBounds`, `OutputId`, `NullifierMap`, `WalletBlock`,
  `WalletTransaction`, `TransparentCoin`, `WalletNote`, `OutgoingNote` and their pool aliases, the
  pool markers, the `KeyIdInterface`, `OutputInterface`, `NoteInterface` and
  `OutgoingNoteInterface` traits, the shard stores, `ShardTrees`, `SHARD_HEIGHT`,
  `MAX_REORG_ALLOWANCE`, `SHARDTREE_CHECKPOINT_ROLLING_WINDOW_SIZE` and `decode_memo_relaxed`.
  `wallet::serialization` holds their codecs.
- `sync`: `SyncState` with the scan scheduler and its codec, `InitialSyncState`, `ScanRange` and
  `ScanPriority`, moved from `pepper_sync::sync` and `pepper_sync::wallet`.
- `config`: `PerformanceLevel`, `SyncConfig`, `TransparentAddressDiscovery`,
  `TransparentAddressDiscoveryScopes` and `DEFAULT_EVENT_CHANNEL_CAPACITY`, moved from
  `pepper_sync::config`.
- `engine`: the operations only a sync engine performs on the wallet data, as sealed traits:
  `KeyIdOps`, `WalletBlockOps`, `WalletTransactionOps`, `TransparentCoinOps`, `WalletNoteOps`,
  `OutgoingNoteOps`, `InitialSyncStateOps` and `SyncStateOps`, with `VerifyEnd` and
  `encode_orchard_receiver`. They were crate-private in `pepper-sync`.
- `price`: `PriceList` and `Price` with their codec, moved from `zingolib-price`.
- `keys`: `KeyId`, `AddressIndex`, `TransparentAddressId`, `decode_address` and
  `decode_unified_address`, moved from `pepper_sync::keys`.
- `serialization`: `invalid_data`, `check_oldest_version` and `collect_unique`, the reader checks
  shared by the codecs.
- `test-features` feature: the wallet type test constructors, as in `pepper-sync`.
- `memo`: moved from `zingolib-memo` 0.1.1
- `status`: moved from `zingolib-status` 0.2.1 (previously `confirmation_status`)
- `serialization::ReadableWriteable`: moved from `zingolib::wallet::traits`. Versioned
  read/write trait now shared by every persisted type in the workspace.
- `chain::ChainType` and `chain::InvalidChainType`: moved from `zingolib::config`.
- `keys`: `UnifiedKeyStore`, `UnifiedAddressId`, `ReceiverSelection` and `KeyError` moved from
  `zingolib::wallet`, and `TransparentScope` moved from `pepper_sync::keys::transparent`.

### Changed

- Renamed from `zingolib-common` to `ekdromoi-common`.
- `keys`: `UnifiedAddress`, `Era` and `UnifiedSpendingKey` come from `zcash_keys`.
- `zcash_client_backend` is used only for the shard codec, with `unstable-serialization`. The
  workspace dependency no longer enables `lightwalletd-tonic`, so the crate builds without tonic.

- `status::ConfirmationStatus`: `read` and `write` are now the `ReadableWriteable` impl.
  Layout 0 is no longer read.
- `keys`: a stored spending key whose sapling `ask` is zero or non-canonical, or a viewing key
  whose sapling `ak` is not a curve point, is rejected instead of panicking inside
  sapling-crypto. Key material lengths past the end of the input are an error, not an
  allocation.
