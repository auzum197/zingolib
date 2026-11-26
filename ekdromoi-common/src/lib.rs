//! Types shared across the ekdromoi workspace: the chain, key material, memo encoding,
//! confirmation status, the wallet state, the sync state and config, the price list and the
//! serialization trait they all use. The crate does no I/O and holds no network code.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

pub mod chain;
pub mod config;
pub mod engine;
pub mod keys;
pub mod memo;
pub mod price;
pub mod protocol;
pub mod serialization;
pub mod status;
pub mod sync;
pub mod wallet;
