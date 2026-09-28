# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `memo`: moved from `zingolib-memo` 0.1.1
- `status`: moved from `zingolib-status` 0.2.1 (previously `confirmation_status`)
- `serialization::ReadableWriteable`: moved from `zingolib::wallet::traits`. Versioned
  read/write trait now shared by every persisted type in the workspace.
- `chain::ChainType` and `chain::InvalidChainType`: moved from `zingolib::config`.
- `keys`: `UnifiedKeyStore`, `UnifiedAddressId`, `ReceiverSelection` and `KeyError` moved from
  `zingolib::wallet`, and `TransparentScope` moved from `pepper_sync::keys::transparent`.

### Changed

- `status::ConfirmationStatus`: `read` and `write` are now the `ReadableWriteable` impl.
  Layout 0 is no longer read.
- `keys`: a stored spending key whose sapling `ask` is zero or non-canonical, or a viewing key
  whose sapling `ak` is not a curve point, is rejected instead of panicking inside
  sapling-crypto. Key material lengths past the end of the input are an error, not an
  allocation.
