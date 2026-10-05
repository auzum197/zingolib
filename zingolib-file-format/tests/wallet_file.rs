//! Conformance tests for the wallet file: pinned vectors from the released binaries,
//! corruption sweeps, golden bytes, layout version pins and round-trip properties.

#[path = "wallet_file/corruption.rs"]
mod corruption;
#[path = "wallet_file/golden_bytes.rs"]
mod golden_bytes;
#[path = "wallet_file/layout_versions.rs"]
mod layout_versions;
#[path = "wallet_file/pinned_releases.rs"]
mod pinned_releases;
#[path = "wallet_file/roundtrip.rs"]
mod roundtrip;
#[path = "wallet_file/support.rs"]
mod support;
