# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `WalletFile`, `WalletFileRef` and `first_addresses`: moved from `zingolib::wallet::disk`.
  `WalletFile::read_encrypted` returns the decoded file with the session that opened it.
- `WalletSettings`: moved from `zingolib::wallet`.
- `encryption`: moved from `zingolib::wallet::encryption`. `EncryptionSession`, `decrypt` and
  the envelope constants are public.
- `legacy`: the pre-32 wallet readers, moved from `zingolib::wallet::legacy` and
  `zingolib::wallet::keys::legacy`.
