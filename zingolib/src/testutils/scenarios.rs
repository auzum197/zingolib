//! Test scenarios on the stateful mock indexer: a chain, a client
//! builder, and funded clients, in the shapes the node-backed suites
//! used.
//!
//! The node-backed suites ran on zcashd regtest, which has no NU6.3, and
//! mined a 625_000_000 zat reward to the faucet in every block, starting
//! with three blocks at launch. The scenarios here keep both: the default
//! chain never activates Ironwood, and [`MockNet::generate_blocks`] pays
//! [`BLOCK_REWARD`] to the faucet's address for the chosen pool.

use std::path::PathBuf;

use tempfile::TempDir;
use zcash_protocol::PoolType;
use zingo_common_components::protocol::ActivationHeights;
use zingo_test_vectors::{
    REG_O_ADDR_FROM_ABANDONART, REG_T_ADDR_FROM_ABANDONART, REG_Z_ADDR_FROM_ABANDONART, seeds,
};

use crate::config::{ChainType, ClientConfig, WalletConfig};
use crate::get_base_address_macro;
use crate::lightclient::LightClient;
use crate::lightclient::error::LightClientError;
use crate::testutils::chain_generics::conduct_chain::ConductChain;
use crate::testutils::default_test_wallet_settings;
use crate::testutils::lightclient::from_inputs::{self, quick_send};
use crate::testutils::lightclient::get_base_address;
use crate::testutils::mock_indexer::{MockChain, MockNet};
use crate::testutils::sync_to_target_height;
use crate::wallet::keys::unified::ReceiverSelection;

pub use crate::testutils::mock_indexer::BLOCK_REWARD;

/// Blocks mined when a scenario chain launches. The node-backed scenarios
/// started at height 3 with three rewards in the faucet.
pub const LAUNCH_BLOCKS: u32 = 3;

/// Every upgrade through NU6.2 active at height 1, NU6.3 never: the chain
/// zcashd served the node-backed suites.
pub fn pre_ironwood_activation_heights() -> ActivationHeights {
    ActivationHeights::builder()
        .set_overwinter(Some(1))
        .set_sapling(Some(1))
        .set_blossom(Some(1))
        .set_heartwood(Some(1))
        .set_canopy(Some(1))
        .set_nu5(Some(1))
        .set_nu6(Some(1))
        .set_nu6_1(Some(1))
        .set_nu6_2(Some(1))
        .set_nu6_3(None)
        .set_nu7(None)
        .build()
}

/// Builds lightclients dialed at one mock indexer, each in its own
/// wallet directory.
pub struct ClientBuilder {
    /// Indexer URI
    pub server_id: http::Uri,
    /// Directory for wallet files
    pub zingo_datadir: TempDir,
    client_number: u8,
}

impl ClientBuilder {
    /// Creates a builder for clients of the indexer at `server_id`.
    pub fn new(server_id: http::Uri, zingo_datadir: TempDir) -> Self {
        ClientBuilder {
            server_id,
            zingo_datadir,
            client_number: 0,
        }
    }

    /// Creates a client config in a fresh wallet directory.
    pub fn make_unique_data_dir_and_create_config(
        &mut self,
        configured_activation_heights: ActivationHeights,
        wallet_config: WalletConfig,
    ) -> ClientConfig {
        self.client_number += 1;
        let conf_path = format!(
            "{}_client_{}",
            self.zingo_datadir.path().to_string_lossy(),
            self.client_number
        );
        self.create_clientconfig(
            PathBuf::from(conf_path),
            configured_activation_heights,
            wallet_config,
        )
    }

    /// Creates a client config in `conf_path`.
    pub fn create_clientconfig(
        &self,
        conf_path: PathBuf,
        configured_activation_heights: ActivationHeights,
        wallet_config: WalletConfig,
    ) -> ClientConfig {
        std::fs::create_dir(&conf_path).unwrap();
        ClientConfig::builder()
            .set_indexer_uri(self.server_id.clone())
            .set_chain_type(ChainType::Regtest(configured_activation_heights))
            .set_wallet_dir(conf_path)
            .set_wallet_config(wallet_config)
            .build()
    }

    /// Builds the faucet, the abandon-art wallet the miner address pays.
    pub async fn build_faucet(
        &mut self,
        overwrite: bool,
        configured_activation_heights: ActivationHeights,
    ) -> LightClient {
        self.build_client(
            WalletConfig::MnemonicPhrase {
                mnemonic_phrase: seeds::ABANDON_ART_SEED.to_string(),
                no_of_accounts: 1.try_into().unwrap(),
                birthday: 1,
                wallet_settings: default_test_wallet_settings(),
            },
            overwrite,
            configured_activation_heights,
        )
        .await
    }

    /// Builds a client with a sapling-only address at index 1, as the
    /// node-backed builder did.
    pub async fn build_client(
        &mut self,
        wallet_config: WalletConfig,
        overwrite: bool,
        configured_activation_heights: ActivationHeights,
    ) -> LightClient {
        let config = self
            .make_unique_data_dir_and_create_config(configured_activation_heights, wallet_config);
        let mut lightclient = LightClient::new(config, overwrite, None).await.unwrap();
        lightclient
            .generate_unified_address(ReceiverSelection::sapling_only(), zip32::AccountId::ZERO)
            .await
            .unwrap();

        lightclient
    }
}

fn faucet_address(mine_to_pool: PoolType) -> &'static str {
    match mine_to_pool {
        PoolType::Transparent => REG_T_ADDR_FROM_ABANDONART,
        PoolType::Shielded(zcash_protocol::ShieldedPool::Sapling) => REG_Z_ADDR_FROM_ABANDONART,
        PoolType::Shielded(
            zcash_protocol::ShieldedPool::Orchard | zcash_protocol::ShieldedPool::Ironwood,
        ) => REG_O_ADDR_FROM_ABANDONART,
    }
}

/// Launches a mock chain that pays the faucet's `mine_to_pool` address in
/// every block, mines the launch block, and returns a client builder for it.
pub async fn custom_clients(
    mine_to_pool: PoolType,
    configured_activation_heights: ActivationHeights,
) -> (MockNet, ClientBuilder) {
    let mut local_net = MockNet::launch_with(MockChain::with_activation_heights(
        configured_activation_heights,
    ))
    .await;
    local_net.set_miner_address(faucet_address(mine_to_pool));
    local_net.generate_blocks(LAUNCH_BLOCKS).await;
    let client_builder = ClientBuilder::new(local_net.indexer_uri(), tempfile::tempdir().unwrap());

    (local_net, client_builder)
}

/// [`custom_clients`] mining to orchard on the pre-Ironwood chain.
pub async fn custom_clients_default() -> (MockNet, ClientBuilder) {
    custom_clients(PoolType::ORCHARD, pre_ironwood_activation_heights()).await
}

/// A synced hospital-museum client on a chain that pays only the faucet.
pub async fn unfunded_client(
    configured_activation_heights: ActivationHeights,
) -> (MockNet, LightClient) {
    let (local_net, mut client_builder) =
        custom_clients(PoolType::ORCHARD, configured_activation_heights).await;
    let mut lightclient = client_builder
        .build_client(
            hospital_museum_config(),
            true,
            configured_activation_heights,
        )
        .await;
    lightclient.sync_and_await().await.unwrap();

    (local_net, lightclient)
}

/// [`unfunded_client`] on the pre-Ironwood chain.
pub async fn unfunded_client_default() -> (MockNet, LightClient) {
    unfunded_client(pre_ironwood_activation_heights()).await
}

/// A synced faucet holding the launch block's reward.
pub async fn faucet(
    mine_to_pool: PoolType,
    configured_activation_heights: ActivationHeights,
) -> (MockNet, LightClient) {
    let (local_net, mut client_builder) =
        custom_clients(mine_to_pool, configured_activation_heights).await;
    let mut faucet = client_builder
        .build_faucet(true, configured_activation_heights)
        .await;
    faucet.sync_and_await().await.unwrap();

    (local_net, faucet)
}

/// [`faucet`] mining to orchard on the pre-Ironwood chain.
pub async fn faucet_default() -> (MockNet, LightClient) {
    faucet(PoolType::ORCHARD, pre_ironwood_activation_heights()).await
}

/// A synced faucet and a synced, unfunded hospital-museum recipient.
pub async fn faucet_recipient(
    mine_to_pool: PoolType,
    configured_activation_heights: ActivationHeights,
) -> (MockNet, LightClient, LightClient) {
    let (local_net, mut client_builder) =
        custom_clients(mine_to_pool, configured_activation_heights).await;
    let mut faucet = client_builder
        .build_faucet(true, configured_activation_heights)
        .await;
    let mut recipient = client_builder
        .build_client(
            hospital_museum_config(),
            true,
            configured_activation_heights,
        )
        .await;
    faucet.sync_and_await().await.unwrap();
    recipient.sync_and_await().await.unwrap();

    (local_net, faucet, recipient)
}

/// [`faucet_recipient`] mining to orchard on the pre-Ironwood chain.
pub async fn faucet_recipient_default() -> (MockNet, LightClient, LightClient) {
    faucet_recipient(PoolType::ORCHARD, pre_ironwood_activation_heights()).await
}

/// [`faucet_recipient`] with the recipient funded from the faucet in the
/// requested pools, confirmed in one block. Returns the funding txids.
pub async fn faucet_funded_recipient(
    orchard_funds: Option<u64>,
    sapling_funds: Option<u64>,
    transparent_funds: Option<u64>,
    mine_to_pool: PoolType,
    configured_activation_heights: ActivationHeights,
) -> (
    MockNet,
    LightClient,
    LightClient,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    let (local_net, mut faucet, mut recipient) =
        faucet_recipient(mine_to_pool, configured_activation_heights).await;
    increase_height_and_wait_for_client(&local_net, &mut faucet, 1)
        .await
        .unwrap();

    let mut fund = async |address: String, funds: Option<u64>| match funds {
        Some(funds) => Some(
            quick_send(&mut faucet, vec![(&address, funds, None)])
                .await
                .unwrap()
                .first()
                .to_string(),
        ),
        None => None,
    };
    let orchard_txid = fund(get_base_address_macro!(recipient, "unified"), orchard_funds).await;
    let sapling_txid = fund(get_base_address_macro!(recipient, "sapling"), sapling_funds).await;
    let transparent_txid = fund(
        get_base_address_macro!(recipient, "transparent"),
        transparent_funds,
    )
    .await;
    increase_height_and_wait_for_client(&local_net, &mut recipient, 1)
        .await
        .unwrap();
    faucet.sync_and_await().await.unwrap();

    (
        local_net,
        faucet,
        recipient,
        orchard_txid,
        sapling_txid,
        transparent_txid,
    )
}

/// [`faucet_funded_recipient`] with orchard funds only, on the pre-Ironwood
/// chain.
pub async fn faucet_funded_recipient_default(
    orchard_funds: u64,
) -> (MockNet, LightClient, LightClient, String) {
    let (local_net, faucet, recipient, orchard_txid, _sapling_txid, _transparent_txid) =
        faucet_funded_recipient(
            Some(orchard_funds),
            None,
            None,
            PoolType::ORCHARD,
            pre_ironwood_activation_heights(),
        )
        .await;

    (local_net, faucet, recipient, orchard_txid.unwrap())
}

/// Sends `value` from `sender` to `recipient`'s `address_pool` address,
/// mines one block, and syncs both clients. Returns the txid.
pub async fn send_value_between_clients_and_sync(
    local_net: &MockNet,
    sender: &mut LightClient,
    recipient: &mut LightClient,
    value: u64,
    address_pool: PoolType,
) -> Result<String, LightClientError> {
    let txid = from_inputs::quick_send(
        sender,
        vec![(
            &get_base_address(recipient, address_pool).await,
            value,
            None,
        )],
    )
    .await
    .unwrap();
    increase_height_and_wait_for_client(local_net, sender, 1).await?;
    recipient.sync_and_await().await?;
    Ok(txid.first().to_string())
}

/// Mines `n` blocks and syncs `client` to the new tip.
pub async fn increase_height_and_wait_for_client(
    local_net: &MockNet,
    client: &mut LightClient,
    n: u32,
) -> Result<(), LightClientError> {
    sync_to_target_height(
        client,
        generate_n_blocks_return_new_height(local_net, n).await,
    )
    .await
}

/// Mines `n` blocks and returns the new tip height.
pub async fn generate_n_blocks_return_new_height(local_net: &MockNet, n: u32) -> u32 {
    let target = local_net.chain_height().await + n;
    local_net.generate_blocks(n).await;
    assert_eq!(local_net.chain_height().await, target);

    target
}

fn hospital_museum_config() -> WalletConfig {
    WalletConfig::MnemonicPhrase {
        mnemonic_phrase: seeds::HOSPITAL_MUSEUM_SEED.to_string(),
        no_of_accounts: 1.try_into().unwrap(),
        birthday: 1,
        wallet_settings: default_test_wallet_settings(),
    }
}

/// The chain-generics environment of the node-backed suite, on the mock:
/// a pre-Ironwood chain whose miner address is the faucet's orchard
/// address.
pub struct RegtestEnvironment {
    /// The mock network.
    pub local_net: MockNet,
    /// Client builder
    pub client_builder: ClientBuilder,
}

impl ConductChain for RegtestEnvironment {
    async fn setup() -> Self {
        let (local_net, client_builder) = custom_clients_default().await;
        RegtestEnvironment {
            local_net,
            client_builder,
        }
    }

    async fn create_faucet(&mut self) -> LightClient {
        self.client_builder
            .build_faucet(false, self.local_net.activation_heights())
            .await
    }

    async fn zingo_config(&mut self) -> ClientConfig {
        self.client_builder.make_unique_data_dir_and_create_config(
            self.local_net.activation_heights(),
            WalletConfig::NewSeed {
                no_of_accounts: 1.try_into().unwrap(),
                chain_height: 1,
                wallet_settings: default_test_wallet_settings(),
            },
        )
    }

    async fn increase_chain_height(&mut self) {
        generate_n_blocks_return_new_height(&self.local_net, 1).await;
    }

    fn lightserver_uri(&self) -> Option<http::Uri> {
        Some(self.local_net.indexer_uri())
    }

    fn confirmation_patience_blocks(&self) -> usize {
        1
    }
}
