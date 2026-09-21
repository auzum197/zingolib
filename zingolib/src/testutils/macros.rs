//! TODO

/// Legacy addresses can be extracted from the receivers, per:
/// <https://zips.z.cash/zip-0316>
// TODO: is this needed as a macro?
// TODO: change unified to orchard as both are unified with orchard-only or sapling-only receiver selections
#[macro_export]
macro_rules! get_base_address_macro {
    ($client:expr, $address_protocol:expr) => {
        match $address_protocol {
            "unified" => {
                let addresses = $client.unified_addresses().await;
                let address = addresses.values().next().unwrap();
                assert!(address.has_orchard());
                address.encode(&$client.chain_type())
            }
            "sapling" => {
                let addresses = $client.unified_addresses().await;
                let address = addresses.values().nth(1).unwrap();
                assert!(!address.has_orchard());
                assert!(address.has_sapling());
                address.encode(&$client.chain_type())
            }
            "transparent" => $client
                .transparent_addresses()
                .await
                .into_values()
                .next()
                .unwrap(),
            _ => "ERROR".to_string(),
        }
    };
}

/// First check that each pools' balance matches an expectation
/// then check that the overall balance as calculated by
/// summing the amounts listed in `tx_summaries` matches the
/// sum of the balances.
#[macro_export]
macro_rules! check_client_balances {
    ($client:ident, i: $ironwood:tt o: $orchard:tt s: $sapling:tt t: $transparent:tt) => {
        let balance = $client
            .account_balance(zip32::AccountId::ZERO)
            .await
            .unwrap();
        assert_eq!(
            balance.total_ironwood_balance.unwrap().into_u64(),
            $ironwood,
            "\ni_balance: {} expectation: {} ",
            balance.total_ironwood_balance.unwrap().into_u64(),
            $ironwood
        );
        $crate::check_client_balances!($client, o: $orchard s: $sapling t: $transparent);
    };
    ($client:ident, o: $orchard:tt s: $sapling:tt t: $transparent:tt) => {
        let balance = $client
            .account_balance(zip32::AccountId::ZERO)
            .await
            .unwrap();
        assert_eq!(
            balance.total_orchard_balance.unwrap().into_u64(),
            $orchard,
            "\no_balance: {} expectation: {} ",
            balance.total_orchard_balance.unwrap().into_u64(),
            $orchard
        );
        assert_eq!(
            balance.total_sapling_balance.unwrap().into_u64(),
            $sapling,
            "\ns_balance: {} expectation: {} ",
            balance.total_sapling_balance.unwrap().into_u64(),
            $sapling
        );
        assert_eq!(
            balance.confirmed_transparent_balance.unwrap().into_u64(),
            $transparent,
            "\nt_balance: {} expectation: {} ",
            balance.confirmed_transparent_balance.unwrap().into_u64(),
            $transparent
        );
    };
}
