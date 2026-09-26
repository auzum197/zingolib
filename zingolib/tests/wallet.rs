#![forbid(unsafe_code)]
mod load_wallet {
    use zingolib::testutils::scenarios::{self, increase_height_and_wait_for_client};
    use zingolib::{get_base_address_macro, testutils::lightclient::from_inputs};

    #[tokio::test]
    async fn verify_old_wallet_uses_server_height_in_send() {
        // An earlier version of zingolib used the _wallet's_ 'height' when
        // constructing transactions.  This worked well enough when the
        // client completed sync prior to sending, but when we introduced
        // interrupting send, it made it immediately obvious that this was
        // the wrong height to use!  The correct height is the
        // "mempool height" which is the server_height + 1
        let (local_net, mut faucet, recipient) = scenarios::faucet_recipient_default().await;
        // Ensure that the client has confirmed spendable funds
        increase_height_and_wait_for_client(&local_net, &mut faucet, 5)
            .await
            .unwrap();

        // Without sync push server forward 2 blocks
        local_net.generate_blocks(2).await;
        let client_fully_scanned_height = faucet
            .wallet()
            .read()
            .await
            .sync_state
            .fully_scanned_height()
            .unwrap();

        // Verify that wallet is still back at 6.
        assert_eq!(client_fully_scanned_height, 8.into());

        // Interrupt generating send
        from_inputs::quick_send(
            &mut faucet,
            vec![(
                &get_base_address_macro!(recipient, "unified"),
                10_000,
                Some("Interrupting sync!!"),
            )],
        )
        .await
        .unwrap();
    }
}
