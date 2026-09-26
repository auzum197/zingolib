//! Types shared across the zingolib workspace: the chain, key material, memo encoding,
//! confirmation status and the serialization trait they all use.

#![warn(missing_docs)]
#![forbid(unsafe_code)]

pub mod chain;
pub mod keys;
pub mod memo;
pub mod serialization;
pub mod status;
