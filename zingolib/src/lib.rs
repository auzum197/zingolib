#![allow(missing_docs)]
#![forbid(unsafe_code)]
//! `ZingoLib`
//! Zingo backend library

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
pub use zingo_common_components::protocol::ActivationHeights;

// This line includes the generated `git_description()` function directly into this scope.
include!(concat!(env!("OUT_DIR"), "/git_description.rs"));
