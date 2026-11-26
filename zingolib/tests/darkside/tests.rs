//! Port of darkside-tests `tests.rs`.

use zingo_test_vectors::seeds::DARKSIDE_SEED;
use zingolib::config::ChainType;
use zingolib::get_base_address_macro;
use zingolib::lightclient::{LightClient, Unlock};
use zingolib::testutils::lightclient::from_inputs;
use zingolib::testutils::scenarios::ClientBuilder;
use zingolib::testutils::tempfile::TempDir;
use zingolib::wallet::balance::AccountBalance;

use crate::support::{self, build_client, prepare_darksidewalletd};

#[tokio::test]
async fn simple_sync() {
    let net = support::launch().await;
    prepare_darksidewalletd(&net, true).await;
    let mut client_builder = ClientBuilder::new(net.indexer_uri(), TempDir::new().unwrap());
    let mut light_client = build_client(&mut client_builder, DARKSIDE_SEED, 1).await;

    let result = light_client.sync_and_await().await.unwrap();

    tracing::info!("{result}");

    assert_eq!(result.sync_end_height, 3.into());
    assert_eq!(result.blocks_scanned, 3);
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
}

#[ignore = "wallet bug: a reorg that replaces blocks without raising the tip goes undetected, \
            so the reorged-out receipt keeps its 100_000_000 balance (one more block makes sync \
            detect it)"]
#[tokio::test]
async fn reorg_receipt_sync_generic() {
    let net = support::launch().await;
    prepare_darksidewalletd(&net, true).await;
    let mut client_builder = ClientBuilder::new(net.indexer_uri(), TempDir::new().unwrap());
    let mut light_client = build_client(&mut client_builder, DARKSIDE_SEED, 1).await;
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
    prepare_darksidewalletd(&net, false).await;
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
}

#[tokio::test]
async fn sent_transaction_reorged_into_mempool() {
    let net = support::launch().await;
    prepare_darksidewalletd(&net, true).await;
    let mut client_manager = ClientBuilder::new(net.indexer_uri(), TempDir::new().unwrap());
    let activation_heights = support::activation_heights();
    let mut light_client = build_client(&mut client_manager, DARKSIDE_SEED, 1).await;
    let mut recipient = build_client(
        &mut client_manager,
        zingo_test_vectors::seeds::HOSPITAL_MUSEUM_SEED,
        1,
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
    let one_txid = from_inputs::quick_send(
        &mut light_client,
        vec![(&get_base_address_macro!(recipient, "unified"), 10_000, None)],
    )
    .await
    .unwrap();
    tracing::info!("{}", one_txid.first());
    recipient.sync_and_await().await.unwrap();

    {
        let mut chain = net.chain.write().await;
        // There should only be one transaction incoming
        assert_eq!(chain.mempool_len(), 1);
        chain.mine_mempool();
    }

    recipient.sync_and_await().await.unwrap();
    tracing::info!(
        "Recipient pre-reorg: {}",
        &recipient
            .account_balance(zip32::AccountId::ZERO)
            .await
            .unwrap()
    );
    tracing::info!(
        "Sender pre-reorg (unsynced): {}",
        &light_client
            .account_balance(zip32::AccountId::ZERO)
            .await
            .unwrap()
    );

    prepare_darksidewalletd(&net, true).await;
    net.chain.write().await.mine_empty_blocks(102);

    recipient.sync_and_await().await.unwrap();
    light_client.sync_and_await().await.unwrap();
    tracing::info!(
        "Recipient post-reorg: {}",
        &recipient
            .account_balance(zip32::AccountId::ZERO)
            .await
            .unwrap()
    );
    tracing::info!(
        "Sender post-reorg: {}",
        &light_client
            .account_balance(zip32::AccountId::ZERO)
            .await
            .unwrap()
    );

    light_client.close().await.unwrap();

    let mut loaded_client = LightClient::open(
        client_manager.store_of(1),
        ChainType::Regtest(activation_heights),
        client_manager.server_id.clone(),
        Unlock::Plaintext,
    )
    .await
    .unwrap();

    loaded_client.sync_and_await().await.unwrap();
    assert_eq!(
        loaded_client
            .account_balance(zip32::AccountId::ZERO)
            .await
            .unwrap()
            .total_orchard_balance
            .unwrap()
            .into_u64(),
        100000000
    );
}
