# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Deprecated

### Added

### Changed

### Removed

## [0.4.0] 2026-06-30

### Deprecated

### Added

- `protocol::ActivationHeights::nu6_3` and
  `protocol::ActivationHeightsBuilder::set_nu6_3`: NU6.3 activation height
  support. `build()` enforces
  `nu6_3 >= nu6_2`.

### Changed

### Removed

## [0.3.1] 2026-06-07

### Deprecated

### Added

- `protocol::BlockHeight`: block height type with arithmetic, ordering, and
  numeric conversions (incl. `H0`, `from_u32`, `saturating_sub`).
- `protocol::TxId`: transaction identifier type with `read`/`write`,
  `is_null`, `NULL`, and `from_bytes`.
- `protocol::ActivationHeights::nu6_2` and
  `protocol::ActivationHeights::set_nu6_2`: NU6.2 activation height support.

### Changed

### Removed

## [0.3.0] 2026-02-26

### Deprecated

### Added

- `protocol::NetworkType`: replaces zebra-chain `NetworkKind` type.
- `protocol::ActivationHeights`: replaces zebra-chain `ConfiguredActivationHeights` type.
- `protocol::ActivationHeightsBuilder`

### Changed

### Removed

- `protocol::activation_heights` mod and `for_test` child module: results in removal of zebra-chain dependency.

## [0.2.0] - 2026-02-12

### Deprecated

### Added

### Changed

- Support for Zebra 4.1.0 through `zebra-chain = "5.0"`
- Bump `tonic` from `0.13` to `0.14`, with `tls-webpki-roots` enabled.

### Removed

## [0.1.0]

NOT PUBLISHED
