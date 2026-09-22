//! TODO

use std::path::PathBuf;

/// TODO: Add Doc Comment Here!
#[must_use]
pub fn get_cargo_manifest_dir() -> PathBuf {
    PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("To be inside a manifested space."))
}
