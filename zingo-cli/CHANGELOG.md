# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Deprecated

### Added
- Optional at-rest encryption of the wallet file with a passphrase
  (Argon2id key derivation + XChaCha20-Poly1305). Supply a passphrase via the
  `--passphrase` flag, the `ZINGO_PASSPHRASE` environment variable, or the
  interactive no-echo prompt shown when opening an encrypted wallet. Tune the
  key-derivation memory with `--kdf-memory-mib <MIB>` (default 64) when creating a
  wallet. New interactive commands: `encrypt` (encrypt an unencrypted wallet or
  rotate the passphrase, always prompting for the passphrase twice with
  confirmation, and accepting an optional `--kdf-memory-mib <MIB>`) and `decrypt`
  (write the wallet in the clear). Existing unencrypted wallets remain fully
  compatible. See the README "Wallet Encryption" section for details.

### Changed

### Removed

## [0.4.0] - 2026-06-10

### Removed
- `regtest` feature: can still use zingo-cli in regtest mode with no features enabled using the '--chain regtest' flag. 
- `tor` flag. tor is no longer supported but will be replaced by nym in the coming release.

## [0.3.0] - 2026-06-05

### Changed
`remove_transaction` command - now only allows transactions with the new `Failed` status to be removed.

### Removed
- `resend` command: see zingolib CHANGELOG.md on `LightClient::resend`
- `send_progress` command

## [0.2.0]

