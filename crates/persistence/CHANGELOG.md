# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `WalletFile` and `WalletFileRef`: moved from `ekdromos::wallet::disk`.
  `WalletFile::read_encrypted` returns the decoded file with the session that opened it.
- `WalletFile::read_any` and `WalletFile::read_encrypted_any`: read a file without knowing its
  chain in advance. A regtest file comes back with the default activation heights.
- `From<&WalletFile> for WalletFileRef`.
- Tests: migration vectors written by the pendrake-watch v0.0.1 and v0.0.2 releases, byte
  mutation sweeps, golden bytes, a struct version pin table and property round trips.

### Changed

- The crate is named `ekdromos-persistence`, formerly `zingolib-file-format`. Its folder is
  `crates/persistence`.
- `WalletFile::read`, `read_any`, `read_encrypted` and `read_encrypted_any` return the new
  `WalletFileError` in place of `io::Error`. Its variants are `UnsupportedVersion`, `WrongChain`,
  `Encryption` and `Decode`.
- Depends on `ekdromos-common` for all wallet data and no longer on `pepper-sync` or
  `zingolib-price`. The crate builds with no network code and no sync engine.

- `WalletFileRef::write` lists transactions in txid order, so a wallet always writes the same
  bytes.
- Readers reject malformed input with `InvalidData` where they could panic or allocate without
  bound: duplicate accounts, addresses, blocks, transactions and outpoints; a birthday or scan
  target below sapling activation; seed entropy of an invalid length; envelope headers that do
  not fit their input.

### Removed

- Every wallet layout before version 41. The first pendrake-watch release wrote 41, so the
  readers for older layouts and their legacy key and note types are gone, along with the
  example wallet files that exercised them.
- `WalletSettings`: moved from `ekdromos::wallet`.
- `encryption`: moved from `ekdromos::wallet::encryption`. `EncryptionSession`, `decrypt` and
  the envelope constants are public.
