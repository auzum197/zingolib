//! Transaction building and broadcast, kept for the test harness only.
//!
//! The wallet is watch-only. The integration tests still need to spend, so the proposal,
//! prover and transmission code lives here behind the `testutils` feature. Nothing in the
//! product API depends on it.

pub mod error;
pub mod proposal;
mod propose;
pub mod receivers;
mod transmit;
