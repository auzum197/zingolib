# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
- `serialization::ReadableWriteable`: moved from `ekdromos::wallet::traits`. Versioned
  read/write trait now shared by every persisted type in the workspace.
- `chain::ChainType` and `chain::InvalidChainType`: moved from `ekdromos::config`.
- `chain::REGTEST_NETWORK`: the regtest activation heights with each upgrade up to NU6.3 active
  from block 1. `chain::LocalNetwork` is a re-export of the `zcash_protocol` type.
- `keys`: `UnifiedKeyStore`, `UnifiedAddressId`, `ReceiverSelection` and `KeyError` moved from
  `ekdromos::wallet`, and `TransparentScope` moved from `pepper_sync::keys::transparent`.

### Changed

- `chain::ChainType::Regtest` holds a `zcash_protocol::local_consensus::LocalNetwork`. It held
  an `ActivationHeights` of `ekdromos-common-components`, and that crate is removed.

- The crate is named `ekdromos-common`, formerly `zingolib-common`.
- `memo::parse_zingolib_memo` is named `memo::parse_ekdromos_memo`.
- `keys`: `UnifiedAddress`, `Era` and `UnifiedSpendingKey` come from `zcash_keys`.
- The `zcash_client_backend` dependency is removed. The crate writes the shards of the wallet
  file with its own codec, and the bytes do not change.

- `status::ConfirmationStatus`: `read` and `write` are now the `ReadableWriteable` impl.
  Layout 0 is no longer read.
- `keys`: a stored spending key whose sapling `ask` is zero or non-canonical, or a viewing key
  whose sapling `ak` is not a curve point, is rejected instead of panicking inside
  sapling-crypto. Key material lengths past the end of the input are an error, not an
  allocation.
