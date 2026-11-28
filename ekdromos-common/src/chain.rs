//! The network a wallet belongs to.

use byteorder::{ReadBytesExt, WriteBytesExt};
use zcash_protocol::consensus::{
    BlockHeight, MAIN_NETWORK, NetworkType, NetworkUpgrade, Parameters, TEST_NETWORK,
};

pub use zcash_protocol::local_consensus::LocalNetwork;

/// Regtest activation heights with each upgrade up to NU6.3 active from block 1.
pub const REGTEST_NETWORK: LocalNetwork = LocalNetwork {
    overwinter: Some(BlockHeight::from_u32(1)),
    sapling: Some(BlockHeight::from_u32(1)),
    blossom: Some(BlockHeight::from_u32(1)),
    heartwood: Some(BlockHeight::from_u32(1)),
    canopy: Some(BlockHeight::from_u32(1)),
    nu5: Some(BlockHeight::from_u32(1)),
    nu6: Some(BlockHeight::from_u32(1)),
    nu6_1: Some(BlockHeight::from_u32(1)),
    nu6_2: Some(BlockHeight::from_u32(1)),
    nu6_3: Some(BlockHeight::from_u32(1)),
    nu7: None,
};

/// The network types a lightclient can connect to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChainType {
    /// Mainnet
    Mainnet,
    /// Testnet
    Testnet,
    /// Regtest
    Regtest(LocalNetwork),
}

impl ChainType {
    /// Reads the wallet-file tag. Regtest activation heights are not stored, so a regtest
    /// wallet comes back with the defaults.
    pub fn read<R: std::io::Read>(mut reader: R) -> std::io::Result<Self> {
        match reader.read_u8()? {
            0 => Ok(ChainType::Mainnet),
            1 => Ok(ChainType::Testnet),
            2 => Ok(ChainType::Regtest(REGTEST_NETWORK)),
            other => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid chain type index stored in wallet file: {other}"),
            )),
        }
    }

    /// Writes the wallet-file tag.
    pub fn write<W: std::io::Write>(&self, mut writer: W) -> std::io::Result<()> {
        writer.write_u8(match self {
            ChainType::Mainnet => 0,
            ChainType::Testnet => 1,
            ChainType::Regtest(_) => 2,
        })
    }
}

impl std::fmt::Display for ChainType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let chain = match self {
            ChainType::Mainnet => "mainnet",
            ChainType::Testnet => "testnet",
            ChainType::Regtest(_) => "regtest",
        };
        write!(f, "{chain}")
    }
}

impl TryFrom<&str> for ChainType {
    type Error = InvalidChainType;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "mainnet" => Ok(ChainType::Mainnet),
            "testnet" => Ok(ChainType::Testnet),
            "regtest" => Ok(ChainType::Regtest(REGTEST_NETWORK)),
            _ => Err(InvalidChainType(value.to_string())),
        }
    }
}

impl Parameters for ChainType {
    fn network_type(&self) -> NetworkType {
        match self {
            ChainType::Mainnet => NetworkType::Main,
            ChainType::Testnet => NetworkType::Test,
            ChainType::Regtest(_) => NetworkType::Regtest,
        }
    }

    fn activation_height(&self, nu: NetworkUpgrade) -> Option<BlockHeight> {
        match self {
            ChainType::Mainnet => MAIN_NETWORK.activation_height(nu),
            ChainType::Testnet => TEST_NETWORK.activation_height(nu),
            ChainType::Regtest(local_network) => local_network.activation_height(nu),
        }
    }
}

/// Invalid chain type.
#[derive(thiserror::Error, Debug)]
#[error("Invalid chain type '{0}'. Expected one of: 'mainnet', 'testnet' or 'regtest'.")]
pub struct InvalidChainType(String);
