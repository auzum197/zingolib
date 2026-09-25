//! On-disk layout of the zingolib wallet file: the versioned container, the readers for
//! every older layout, and the passphrase-encrypted envelope around it.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

pub mod encryption;
mod file;
pub mod legacy;
mod settings;

pub use file::{WalletFile, WalletFileRef, first_addresses};
pub use settings::WalletSettings;
