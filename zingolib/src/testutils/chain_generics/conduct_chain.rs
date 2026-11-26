//! A Lightclient test may involve hosting a server to send data to the LightClient. This trait can be asked to set simple scenarios where a mock LightServer sends data showing a note to a LightClient, the LightClient updates and responds by sending the note, and the Lightserver accepts the transaction and rebroadcasts it...
//! The implementors are
//! `MockNet` and `RegtestEnvironment`, which run on the in-process mock indexer
//! `NetworkedTestEnvironment`, which follows a live testnet indexer
//! darkside, a mode for the lightserver which mocks zcashd. search 'impl ConductChain for DarksideEnvironment

use crate::config::{ChainType, WalletConfig};
use crate::get_base_address_macro;
use crate::lightclient::store::FileStore;
use crate::lightclient::{Encryption, LightClient};
use crate::testutils::default_test_wallet_settings;
use crate::testutils::lightclient::from_inputs;
use crate::wallet::keys::unified::ReceiverSelection;

#[allow(async_fn_in_trait)]
#[allow(opaque_hidden_inferred_bound)]
/// a trait (capability) for operating a server.
/// delegates client setup, because different mock servers require different client configuration
/// currently, the server conductor is limited to adding to the mock blockchain linearly (bump chain)
pub trait ConductChain {
    /// set up the test chain
    async fn setup() -> Self;

    /// used to connect to server via grpc
    fn lightserver_uri(&self) -> Option<http::Uri>;

    /// builds a faucet (funded from mining)
    async fn create_faucet(&mut self) -> LightClient;

    /// a fresh store, the chain and the indexer that a new client of this server uses
    fn client_target(&mut self) -> (FileStore, ChainType, http::Uri);

    /// builds an empty client
    async fn create_client(&mut self) -> LightClient {
        let (store, chain_type, indexer_uri) = self.client_target();
        let mut lightclient = LightClient::create(
            store,
            chain_type,
            indexer_uri,
            WalletConfig::NewSeed {
                no_of_accounts: 1.try_into().unwrap(),
                chain_height: 1,
                wallet_settings: default_test_wallet_settings(),
            },
            Encryption::None,
        )
        .await
        .unwrap();
        lightclient
            .generate_unified_address(ReceiverSelection::sapling_only(), zip32::AccountId::ZERO)
            .await
            .unwrap();

        lightclient
    }

    /// moves the chain tip forward, creating 1 new block
    /// and confirming transactions that were received by the server
    async fn increase_chain_height(&mut self);

    /// builds a client and funds it in orchard and syncs it
    async fn fund_client_orchard(&mut self, value: u64) -> LightClient {
        let mut faucet = self.create_faucet().await;
        let mut recipient = self.create_client().await;

        self.increase_chain_height().await;
        faucet.sync_and_await().await.unwrap();

        from_inputs::quick_send(
            &mut faucet,
            vec![(
                (get_base_address_macro!(recipient, "unified")).as_str(),
                value,
                None,
            )],
        )
        .await
        .unwrap();

        self.increase_chain_height().await;

        recipient.sync_and_await().await.unwrap();

        recipient
    }

    /// how many blocks of leeway to allow the chain to Confirm a transaction
    fn confirmation_patience_blocks(&self) -> usize;
}
