# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `WalletFile` and `WalletFileRef`: moved from `zingolib::wallet::disk`.
  `WalletFile::read_encrypted` returns the decoded file with the session that opened it.
- `WalletFile::read_any` and `WalletFile::read_encrypted_any`: read a file without knowing its
  chain in advance. A regtest file comes back with the default activation heights.
- `From<&WalletFile> for WalletFileRef`.

### Removed

- Every wallet layout before version 41. The first pendrake-watch release wrote 41, so the
  readers for older layouts and their legacy key and note types are gone, along with the
  example wallet files that exercised them.
- `WalletSettings`: moved from `zingolib::wallet`.
- `encryption`: moved from `zingolib::wallet::encryption`. `EncryptionSession`, `decrypt` and
  the envelope constants are public.
