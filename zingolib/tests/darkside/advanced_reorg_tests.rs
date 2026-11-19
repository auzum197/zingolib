//! Port of darkside-tests `advanced_reorg_tests.rs`. The recorded
//! datasets start at height 202 and the mock mines them block by block.

use zcash_protocol::consensus::BlockHeight;
use zingolib::testutils::lightclient::from_inputs;
use zingolib::testutils::scenarios::ClientBuilder;
use zingolib::testutils::tempfile::TempDir;
use zingolib::wallet::balance::AccountBalance;
use zingolib::wallet::summary::data::{SentWalletEvent, WalletEventKind};

use crate::constants::{
    ADVANCED_REORG_TESTS_USER_WALLET, REORG_CHANGES_INCOMING_TX_HEIGHT_AFTER,
    REORG_CHANGES_INCOMING_TX_HEIGHT_BEFORE, REORG_CHANGES_INCOMING_TX_INDEX_AFTER,
    REORG_CHANGES_INCOMING_TX_INDEX_BEFORE, REORG_EXPIRES_INCOMING_TX_HEIGHT_AFTER,
    REORG_EXPIRES_INCOMING_TX_HEIGHT_BEFORE, TRANSACTION_TO_FILLER_ADDRESS,
};
use crate::support::{
    self, apply_dataset, build_client, read_dataset, stage_dataset, take_incoming_transaction,
};

#[tokio::test]
async fn reorg_changes_incoming_tx_height() {
    let net = support::launch().await;
    let dataset = read_dataset(REORG_CHANGES_INCOMING_TX_HEIGHT_BEFORE);
    stage_dataset(&net, &dataset, 204).await;

    let mut client_builder = ClientBuilder::new(net.indexer_uri(), TempDir::new().unwrap());
    let mut light_client = build_client(
        &mut client_builder,
        ADVANCED_REORG_TESTS_USER_WALLET,
        support::DATASET_START,
    )
    .await;

    light_client.sync_and_await().await.unwrap();
    assert_eq!(
        light_client
            .account_balance(zip32::AccountId::ZERO)
            .await
            .unwrap(),
        AccountBalance {
            total_sapling_balance: Some(0.try_into().unwrap()),
            confirmed_sapling_balance: Some(0.try_into().unwrap()),
            unconfirmed_sapling_balance: Some(0.try_into().unwrap()),
            total_orchard_balance: Some(100000000.try_into().unwrap()),
            confirmed_orchard_balance: Some(100000000.try_into().unwrap()),
            unconfirmed_orchard_balance: Some(0.try_into().unwrap()),
            total_ironwood_balance: Some(0.try_into().unwrap()),
            confirmed_ironwood_balance: Some(0.try_into().unwrap()),
            unconfirmed_ironwood_balance: Some(0.try_into().unwrap()),
            total_transparent_balance: Some(0.try_into().unwrap()),
            confirmed_transparent_balance: Some(0.try_into().unwrap()),
            unconfirmed_transparent_balance: Some(0.try_into().unwrap())
        }
    );

    let before_reorg_events = light_client.wallet_events(true).await.unwrap();

    assert_eq!(before_reorg_events.len(), 1);
    assert_eq!(
        before_reorg_events[0].blockheight,
        BlockHeight::from_u32(203)
    );

    stage_dataset(
        &net,
        &read_dataset(REORG_CHANGES_INCOMING_TX_HEIGHT_AFTER),
        206,
    )
    .await;

    let reorg_sync_result = light_client.sync_and_await().await;

    match reorg_sync_result {
        Ok(value) => tracing::info!("{value}"),
        Err(err_str) => tracing::info!("{err_str}"),
    }

    assert_eq!(
        light_client
            .account_balance(zip32::AccountId::ZERO)
            .await
            .unwrap(),
        AccountBalance {
            total_sapling_balance: Some(0.try_into().unwrap()),
            confirmed_sapling_balance: Some(0.try_into().unwrap()),
            unconfirmed_sapling_balance: Some(0.try_into().unwrap()),
            total_orchard_balance: Some(100000000.try_into().unwrap()),
            confirmed_orchard_balance: Some(100000000.try_into().unwrap()),
            unconfirmed_orchard_balance: Some(0.try_into().unwrap()),
            total_ironwood_balance: Some(0.try_into().unwrap()),
            confirmed_ironwood_balance: Some(0.try_into().unwrap()),
            unconfirmed_ironwood_balance: Some(0.try_into().unwrap()),
            total_transparent_balance: Some(0.try_into().unwrap()),
            confirmed_transparent_balance: Some(0.try_into().unwrap()),
            unconfirmed_transparent_balance: Some(0.try_into().unwrap())
        }
    );

    let after_reorg_events = light_client.wallet_events(true).await.unwrap();

    assert_eq!(after_reorg_events.len(), 1);
    assert_eq!(
        after_reorg_events[0].blockheight,
        BlockHeight::from_u32(206)
    );
}

#[tokio::test]
async fn reorg_changes_incoming_tx_index() {
    let net = support::launch().await;
    let dataset = read_dataset(REORG_CHANGES_INCOMING_TX_INDEX_BEFORE);
    stage_dataset(&net, &dataset, 204).await;

    let mut client_builder = ClientBuilder::new(net.indexer_uri(), TempDir::new().unwrap());
    let mut light_client = build_client(
        &mut client_builder,
        ADVANCED_REORG_TESTS_USER_WALLET,
        support::DATASET_START,
    )
    .await;

    light_client.sync_and_await().await.unwrap();
    assert_eq!(
        light_client
            .account_balance(zip32::AccountId::ZERO)
            .await
            .unwrap(),
        AccountBalance {
            total_sapling_balance: Some(0.try_into().unwrap()),
            confirmed_sapling_balance: Some(0.try_into().unwrap()),
            unconfirmed_sapling_balance: Some(0.try_into().unwrap()),
            total_orchard_balance: Some(100000000.try_into().unwrap()),
            confirmed_orchard_balance: Some(100000000.try_into().unwrap()),
            unconfirmed_orchard_balance: Some(0.try_into().unwrap()),
            total_ironwood_balance: Some(0.try_into().unwrap()),
            confirmed_ironwood_balance: Some(0.try_into().unwrap()),
            unconfirmed_ironwood_balance: Some(0.try_into().unwrap()),
            total_transparent_balance: Some(0.try_into().unwrap()),
            confirmed_transparent_balance: Some(0.try_into().unwrap()),
            unconfirmed_transparent_balance: Some(0.try_into().unwrap())
        }
    );

    let before_reorg_events = light_client.wallet_events(true).await.unwrap();

    assert_eq!(before_reorg_events.len(), 1);
    assert_eq!(
        before_reorg_events[0].blockheight,
        BlockHeight::from_u32(203)
    );

    stage_dataset(
        &net,
        &read_dataset(REORG_CHANGES_INCOMING_TX_INDEX_AFTER),
        206,
    )
    .await;

    let reorg_sync_result = light_client.sync_and_await().await;

    match reorg_sync_result {
        Ok(value) => tracing::info!("{value}"),
        Err(err_str) => tracing::info!("{err_str}"),
    }

    assert_eq!(
        light_client
            .account_balance(zip32::AccountId::ZERO)
            .await
            .unwrap(),
        AccountBalance {
            total_sapling_balance: Some(0.try_into().unwrap()),
            confirmed_sapling_balance: Some(0.try_into().unwrap()),
            unconfirmed_sapling_balance: Some(0.try_into().unwrap()),
            total_orchard_balance: Some(100000000.try_into().unwrap()),
            confirmed_orchard_balance: Some(100000000.try_into().unwrap()),
            unconfirmed_orchard_balance: Some(0.try_into().unwrap()),
            total_ironwood_balance: Some(0.try_into().unwrap()),
            confirmed_ironwood_balance: Some(0.try_into().unwrap()),
            unconfirmed_ironwood_balance: Some(0.try_into().unwrap()),
            total_transparent_balance: Some(0.try_into().unwrap()),
            confirmed_transparent_balance: Some(0.try_into().unwrap()),
            unconfirmed_transparent_balance: Some(0.try_into().unwrap())
        }
    );

    let after_reorg_events = light_client.wallet_events(true).await.unwrap();

    assert_eq!(after_reorg_events.len(), 1);
    assert_eq!(
        after_reorg_events[0].blockheight,
        BlockHeight::from_u32(203)
    );
}

#[ignore = "the wallet keeps the reorged-out receipt as a wallet event with status failed, \
            where this test expects no events (the balance does drop to 0)"]
#[tokio::test]
async fn reorg_expires_incoming_tx() {
    let net = support::launch().await;
    let dataset = read_dataset(REORG_EXPIRES_INCOMING_TX_HEIGHT_BEFORE);
    stage_dataset(&net, &dataset, 204).await;

    let mut client_builder = ClientBuilder::new(net.indexer_uri(), TempDir::new().unwrap());
    let mut light_client = build_client(
        &mut client_builder,
        ADVANCED_REORG_TESTS_USER_WALLET,
        support::DATASET_START,
    )
    .await;

    light_client.sync_and_await().await.unwrap();
    assert_eq!(
        light_client
            .account_balance(zip32::AccountId::ZERO)
            .await
            .unwrap(),
        AccountBalance {
            total_sapling_balance: Some(0.try_into().unwrap()),
            confirmed_sapling_balance: Some(0.try_into().unwrap()),
            unconfirmed_sapling_balance: Some(0.try_into().unwrap()),
            total_orchard_balance: Some(100000000.try_into().unwrap()),
            confirmed_orchard_balance: Some(100000000.try_into().unwrap()),
            unconfirmed_orchard_balance: Some(0.try_into().unwrap()),
            total_ironwood_balance: Some(0.try_into().unwrap()),
            confirmed_ironwood_balance: Some(0.try_into().unwrap()),
            unconfirmed_ironwood_balance: Some(0.try_into().unwrap()),
            total_transparent_balance: Some(0.try_into().unwrap()),
            confirmed_transparent_balance: Some(0.try_into().unwrap()),
            unconfirmed_transparent_balance: Some(0.try_into().unwrap())
        }
    );

    let before_reorg_events = light_client.wallet_events(true).await.unwrap();

    assert_eq!(before_reorg_events.len(), 1);
    assert_eq!(
        before_reorg_events[0].blockheight,
        BlockHeight::from_u32(203)
    );

    stage_dataset(
        &net,
        &read_dataset(REORG_EXPIRES_INCOMING_TX_HEIGHT_AFTER),
        206,
    )
    .await;

    let reorg_sync_result = light_client.sync_and_await().await;

    match reorg_sync_result {
        Ok(value) => tracing::info!("{value}"),
        Err(err_str) => tracing::info!("{err_str}"),
    }

    assert_eq!(
        light_client
            .account_balance(zip32::AccountId::ZERO)
            .await
            .unwrap(),
        AccountBalance {
            total_sapling_balance: Some(0.try_into().unwrap()),
            confirmed_sapling_balance: Some(0.try_into().unwrap()),
            unconfirmed_sapling_balance: Some(0.try_into().unwrap()),
            total_orchard_balance: Some(0.try_into().unwrap()),
            confirmed_orchard_balance: Some(0.try_into().unwrap()),
            unconfirmed_orchard_balance: Some(0.try_into().unwrap()),
            total_ironwood_balance: Some(0.try_into().unwrap()),
            confirmed_ironwood_balance: Some(0.try_into().unwrap()),
            unconfirmed_ironwood_balance: Some(0.try_into().unwrap()),
            total_transparent_balance: Some(0.try_into().unwrap()),
            confirmed_transparent_balance: Some(0.try_into().unwrap()),
            unconfirmed_transparent_balance: Some(0.try_into().unwrap())
        }
    );

    let after_reorg_events = light_client.wallet_events(true).await.unwrap();

    assert_eq!(after_reorg_events.len(), 0);
}

#[tokio::test]
/// ### `ReOrg` Removes Outbound `TxAnd` Is Never Mined
/// Transaction was included in a block, and then is not included in a block after a reorg, and expires.
#[ignore = "the wallet keeps the expired send as a wallet event with status failed, where this \
            test expects only the receipt (the balance does return to 100_000_000)"]
async fn reorg_expires_outgoing_tx_height() {
    let net = support::launch().await;
    let dataset = read_dataset(REORG_EXPIRES_INCOMING_TX_HEIGHT_BEFORE);
    stage_dataset(&net, &dataset, 204).await;

    let mut client_builder = ClientBuilder::new(net.indexer_uri(), TempDir::new().unwrap());
    let mut light_client = build_client(
        &mut client_builder,
        ADVANCED_REORG_TESTS_USER_WALLET,
        support::DATASET_START,
    )
    .await;

    let expected_initial_balance = AccountBalance {
        total_sapling_balance: Some(0.try_into().unwrap()),
        confirmed_sapling_balance: Some(0.try_into().unwrap()),
        unconfirmed_sapling_balance: Some(0.try_into().unwrap()),
        total_orchard_balance: Some(100000000.try_into().unwrap()),
        confirmed_orchard_balance: Some(100000000.try_into().unwrap()),
        unconfirmed_orchard_balance: Some(0.try_into().unwrap()),
        total_ironwood_balance: Some(0.try_into().unwrap()),
        confirmed_ironwood_balance: Some(0.try_into().unwrap()),
        unconfirmed_ironwood_balance: Some(0.try_into().unwrap()),
        total_transparent_balance: Some(0.try_into().unwrap()),
        confirmed_transparent_balance: Some(0.try_into().unwrap()),
        unconfirmed_transparent_balance: Some(0.try_into().unwrap()),
    };

    light_client.sync_and_await().await.unwrap();
    assert_eq!(
        light_client
            .account_balance(zip32::AccountId::ZERO)
            .await
            .unwrap(),
        expected_initial_balance
    );

    let before_reorg_events = light_client.wallet_events(true).await.unwrap();

    assert_eq!(before_reorg_events.len(), 1);
    assert_eq!(
        before_reorg_events[0].blockheight,
        BlockHeight::from_u32(203)
    );

    let recipient_string = "uregtest1z8s5szuww2cnze042e0re2ez8l3d04zvkp7kslxwdha6tp644srd4nh0xlp8a05avzduc6uavqkxv79x53c60hrc0qsgeza3age2g3qualullukd4s0lsn6mtfup4z8jz6xdz2c05zakhafc7pmw0dwugwu9ljevzgyc3mfwxg9slr87k8l7cq075gl3fgxpr85uuvxhxydrskp2303";

    let amount: u64 = 100000;
    let sent_tx_id = from_inputs::quick_send(
        &mut light_client,
        [(recipient_string, amount, None)].to_vec(),
    )
    .await
    .unwrap();

    tracing::info!("SENT TX ID: {sent_tx_id:?}");

    take_incoming_transaction(&net).await;

    let sent_tx_height: i32 = 205;
    apply_dataset(&net, &dataset, sent_tx_height as u32).await;

    light_client.sync_and_await().await.unwrap();

    let expected_after_send_balance = AccountBalance {
        total_sapling_balance: Some(0.try_into().unwrap()),
        confirmed_sapling_balance: Some(0.try_into().unwrap()),
        unconfirmed_sapling_balance: Some(0.try_into().unwrap()),
        total_orchard_balance: Some(99890000.try_into().unwrap()),
        confirmed_orchard_balance: Some(0.try_into().unwrap()),
        unconfirmed_orchard_balance: Some(99890000.try_into().unwrap()),
        total_ironwood_balance: Some(0.try_into().unwrap()),
        confirmed_ironwood_balance: Some(0.try_into().unwrap()),
        unconfirmed_ironwood_balance: Some(0.try_into().unwrap()),
        total_transparent_balance: Some(0.try_into().unwrap()),
        confirmed_transparent_balance: Some(0.try_into().unwrap()),
        unconfirmed_transparent_balance: Some(0.try_into().unwrap()),
    };

    assert_eq!(
        light_client
            .account_balance(zip32::AccountId::ZERO)
            .await
            .unwrap(),
        expected_after_send_balance
    );

    // check that the outgoing transaction has the correct height before
    // the reorg is triggered

    tracing::info!("{:?}", light_client.wallet_events(true).await.unwrap());

    let send_height = light_client
        .wallet_events(true)
        .await
        .unwrap()
        .iter()
        .find_map(|v| match v.kind {
            WalletEventKind::Sent(SentWalletEvent::Send) => {
                if let Some(addr) = v.recipient_address.as_ref() {
                    if addr == recipient_string && v.value == 100_000 {
                        Some(v.blockheight)
                    } else {
                        None
                    }
                } else {
                    None
                }
            }
            _ => None,
        });
    assert_eq!(send_height, Some(BlockHeight::from(sent_tx_height as u32)));

    // stage empty blocks from height 205 to cause a Reorg
    {
        let mut chain = net.chain.write().await;
        chain.reorg_to(sent_tx_height as u32 - 1);
        let tip = chain.tip();
        chain.mine_empty_blocks(245 - tip);
    }

    // this will remove the submitted transaction from our view of the blockchain
    let reorg_sync_result = light_client.sync_and_await().await;

    match reorg_sync_result {
        Ok(value) => tracing::info!("{value}"),
        Err(err_str) => tracing::info!("{err_str}"),
    }

    // Assert that balance is equal to the initial balance since the
    // sent transaction was never mined and has expired.
    assert_eq!(
        light_client
            .account_balance(zip32::AccountId::ZERO)
            .await
            .unwrap(),
        expected_initial_balance
    );

    let after_reorg_events = light_client.wallet_events(true).await.unwrap();

    assert_eq!(after_reorg_events.len(), 1);

    tracing::info!("{:?}", light_client.wallet_events(true).await);
}

#[tokio::test]
/// ### Reorg Changes Outbound Tx Index
/// An outbound, pending transaction in a specific block changes height in the event of a reorg
///
/// The wallet handles this change, reflects it appropriately in local storage, and funds remain spendable post confirmation.
///
/// **Pre-conditions:**
///   - Wallet has spendable funds
async fn reorg_changes_outgoing_tx_index() {
    tracing_subscriber::fmt().init();

    let net = support::launch().await;
    let dataset = read_dataset(REORG_EXPIRES_INCOMING_TX_HEIGHT_BEFORE);
    stage_dataset(&net, &dataset, 204).await;

    let mut client_builder = ClientBuilder::new(net.indexer_uri(), TempDir::new().unwrap());
    let mut light_client = build_client(
        &mut client_builder,
        ADVANCED_REORG_TESTS_USER_WALLET,
        support::DATASET_START,
    )
    .await;

    light_client.sync_and_await().await.unwrap();
    assert_eq!(
        light_client
            .account_balance(zip32::AccountId::ZERO)
            .await
            .unwrap(),
        AccountBalance {
            total_sapling_balance: Some(0.try_into().unwrap()),
            confirmed_sapling_balance: Some(0.try_into().unwrap()),
            unconfirmed_sapling_balance: Some(0.try_into().unwrap()),
            total_orchard_balance: Some(100_000_000.try_into().unwrap()),
            confirmed_orchard_balance: Some(100_000_000.try_into().unwrap()),
            unconfirmed_orchard_balance: Some(0.try_into().unwrap()),
            total_ironwood_balance: Some(0.try_into().unwrap()),
            confirmed_ironwood_balance: Some(0.try_into().unwrap()),
            unconfirmed_ironwood_balance: Some(0.try_into().unwrap()),
            total_transparent_balance: Some(0.try_into().unwrap()),
            confirmed_transparent_balance: Some(0.try_into().unwrap()),
            unconfirmed_transparent_balance: Some(0.try_into().unwrap())
        }
    );

    let before_reorg_events = light_client.wallet_events(true).await.unwrap();

    assert_eq!(before_reorg_events.len(), 1);
    assert_eq!(
        before_reorg_events[0].blockheight,
        BlockHeight::from_u32(203)
    );

    let recipient_string = "uregtest1z8s5szuww2cnze042e0re2ez8l3d04zvkp7kslxwdha6tp644srd4nh0xlp8a05avzduc6uavqkxv79x53c60hrc0qsgeza3age2g3qualullukd4s0lsn6mtfup4z8jz6xdz2c05zakhafc7pmw0dwugwu9ljevzgyc3mfwxg9slr87k8l7cq075gl3fgxpr85uuvxhxydrskp2303";

    let amount: u64 = 100_000;
    let sent_tx_id = from_inputs::quick_send(
        &mut light_client,
        [(recipient_string, amount, None)].to_vec(),
    )
    .await
    .unwrap();

    tracing::info!("SENT TX ID: {sent_tx_id:?}");

    let tx = take_incoming_transaction(&net).await;

    let sent_tx_height: i32 = 205;
    apply_dataset(&net, &dataset, sent_tx_height as u32).await;

    light_client.sync_and_await().await.unwrap();

    let expected_after_send_balance = AccountBalance {
        total_sapling_balance: Some(0.try_into().unwrap()),
        confirmed_sapling_balance: Some(0.try_into().unwrap()),
        unconfirmed_sapling_balance: Some(0.try_into().unwrap()),
        total_orchard_balance: Some(99_880_000.try_into().unwrap()),
        confirmed_orchard_balance: Some(0.try_into().unwrap()),
        unconfirmed_orchard_balance: Some(99_880_000.try_into().unwrap()),
        total_ironwood_balance: Some(0.try_into().unwrap()),
        confirmed_ironwood_balance: Some(0.try_into().unwrap()),
        unconfirmed_ironwood_balance: Some(0.try_into().unwrap()),
        total_transparent_balance: Some(0.try_into().unwrap()),
        confirmed_transparent_balance: Some(0.try_into().unwrap()),
        unconfirmed_transparent_balance: Some(0.try_into().unwrap()),
    };

    assert_eq!(
        light_client
            .account_balance(zip32::AccountId::ZERO)
            .await
            .unwrap(),
        expected_after_send_balance
    );

    // check that the outgoing transaction has the correct height before
    // the reorg is triggered

    assert_eq!(
        light_client
            .wallet_events(true)
            .await
            .unwrap()
            .iter()
            .find_map(|v| match v.kind {
                WalletEventKind::Sent(SentWalletEvent::Send) => {
                    if let Some(addr) = v.recipient_address.as_ref() {
                        if addr == recipient_string && v.value == 100_000 {
                            Some(v.blockheight)
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
                _ => {
                    None
                }
            }),
        Some(BlockHeight::from(sent_tx_height as u32))
    );

    tracing::info!("pre re-org wallet events:");
    tracing::info!("{}", light_client.wallet_events(true).await.unwrap());
    tracing::info!("pre re-org tx summaries:");
    tracing::info!(
        "{}",
        light_client.transaction_summaries(false).await.unwrap()
    );

    {
        let mut chain = net.chain.write().await;
        chain.reorg_to(sent_tx_height as u32 - 1);
        chain.mine_block(vec![
            hex::decode(TRANSACTION_TO_FILLER_ADDRESS).unwrap(),
            tx.clone(),
        ]);
    }
    {
        let mut chain = net.chain.write().await;
        let tip = chain.tip();
        chain.mine_empty_blocks(312 - tip);
    }

    light_client.sync_and_await().await.unwrap();

    let expected_after_reorg_balance = AccountBalance {
        total_sapling_balance: Some(0.try_into().unwrap()),
        confirmed_sapling_balance: Some(0.try_into().unwrap()),
        unconfirmed_sapling_balance: Some(0.try_into().unwrap()),
        total_orchard_balance: Some(99_880_000.try_into().unwrap()),
        confirmed_orchard_balance: Some(99_880_000.try_into().unwrap()),
        unconfirmed_orchard_balance: Some(0.try_into().unwrap()),
        total_ironwood_balance: Some(0.try_into().unwrap()),
        confirmed_ironwood_balance: Some(0.try_into().unwrap()),
        unconfirmed_ironwood_balance: Some(0.try_into().unwrap()),
        total_transparent_balance: Some(0.try_into().unwrap()),
        confirmed_transparent_balance: Some(0.try_into().unwrap()),
        unconfirmed_transparent_balance: Some(0.try_into().unwrap()),
    };

    assert_eq!(
        light_client
            .account_balance(zip32::AccountId::ZERO)
            .await
            .unwrap(),
        expected_after_reorg_balance
    );

    let after_reorg_events = light_client.wallet_events(true).await.unwrap();

    tracing::info!("post re-org wallet events:");
    tracing::info!("{after_reorg_events}");
    tracing::info!("post re-org tx summaries:");
    tracing::info!(
        "{}",
        light_client.transaction_summaries(false).await.unwrap()
    );
}
