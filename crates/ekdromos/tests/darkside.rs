//! The darkside-tests suites, on the mock indexer in place of
//! darksidewalletd.

#[path = "darkside/advanced_reorg_tests.rs"]
mod advanced_reorg_tests;
#[path = "darkside/chain_generics.rs"]
mod chain_generics;
#[path = "darkside/constants.rs"]
mod constants;
#[path = "darkside/support.rs"]
mod support;
#[path = "darkside/tests.rs"]
mod tests;
