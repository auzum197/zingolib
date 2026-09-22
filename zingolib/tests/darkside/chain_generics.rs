//! Port of darkside-tests `src/chain_generics.rs`.

use proptest::proptest;
use tokio::runtime::Runtime;

use zingolib::testutils::chain_generics::fixtures;
use zingolib::testutils::int_to_pooltype;
use zingolib::testutils::int_to_shieldedprotocol;

use crate::support::DarksideEnvironment;

proptest! {
    #![proptest_config(proptest::test_runner::Config::with_cases(4))]
    #[test]
    fn any_source_sends_to_any_receiver_darkside(send_value in 0..50_000u64, change_value in 0..10_000u64, sender_protocol in 1..2, receiver_pool in 1..2) {
        // note: this darkside test does not check the mempool
        Runtime::new().unwrap().block_on(async {
            fixtures::any_source_sends_to_any_receiver::<DarksideEnvironment>(int_to_shieldedprotocol(sender_protocol), int_to_pooltype(receiver_pool), send_value, change_value, false).await;
        });
     }
    #[test]
    fn any_source_sends_to_any_receiver_0_change_darkside(send_value in 0..50_000u64, sender_protocol in 1..2, receiver_pool in 1..2) {
        Runtime::new().unwrap().block_on(async {
            fixtures::any_source_sends_to_any_receiver::<DarksideEnvironment>(int_to_shieldedprotocol(sender_protocol), int_to_pooltype(receiver_pool), send_value, 0, false).await;
        });
     }
}
