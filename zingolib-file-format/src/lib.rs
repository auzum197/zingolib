//! On-disk layout of the zingolib wallet file: the container and the passphrase-encrypted
//! envelope around it.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

pub mod encryption;
mod file;
mod settings;

pub use file::{WalletFile, WalletFileRef};
pub use settings::WalletSettings;
