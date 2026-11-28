#![allow(missing_docs)]
#![forbid(unsafe_code)]
//! `Ekdromos`
//! Ekdromos wallet library

pub mod config;
pub mod data;
pub mod lightclient;
pub mod price;
pub mod utils;
pub mod wallet;

#[cfg(any(test, feature = "testutils"))]
pub mod mocks;
#[cfg(any(test, feature = "testutils"))]
pub mod testutils;

pub use chain_router;
pub use lightwallet_grpc_client::GrpcIndexer;
pub use pepper_sync::strategy;
