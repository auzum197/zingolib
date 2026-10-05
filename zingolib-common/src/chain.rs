//! The network a wallet belongs to.

use byteorder::{ReadBytesExt, WriteBytesExt};
use zcash_protocol::consensus::{
    BlockHeight, MAIN_NETWORK, NetworkType, NetworkUpgrade, Parameters, TEST_NETWORK,
};
use zingo_common_components::protocol::ActivationHeights;

/// The network types a lightclient can connect to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChainType {
    /// Mainnet
    Mainnet,
    /// Testnet
    Testnet,
    /// Regtest
    Regtest(ActivationHeights),
}

impl ChainType {
    /// Reads the wallet-file tag. Regtest activation heights are not stored, so a regtest
    /// wallet comes back with the defaults.
    pub fn read<R: std::io::Read>(mut reader: R) -> std::io::Result<Self> {
        match reader.read_u8()? {
            0 => Ok(ChainType::Mainnet),
            1 => Ok(ChainType::Testnet),
            2 => Ok(ChainType::Regtest(ActivationHeights::default())),
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
            "regtest" => Ok(ChainType::Regtest(ActivationHeights::default())),
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
            ChainType::Regtest(activation_heights) => match nu {
                NetworkUpgrade::Nu6_3 => activation_heights.nu6_3().map(BlockHeight::from_u32),
                NetworkUpgrade::Overwinter => {
                    activation_heights.overwinter().map(BlockHeight::from_u32)
                }
                NetworkUpgrade::Sapling => activation_heights.sapling().map(BlockHeight::from_u32),
                NetworkUpgrade::Blossom => activation_heights.blossom().map(BlockHeight::from_u32),
                NetworkUpgrade::Heartwood => {
                    activation_heights.heartwood().map(BlockHeight::from_u32)
                }
                NetworkUpgrade::Canopy => activation_heights.canopy().map(BlockHeight::from_u32),
                NetworkUpgrade::Nu5 => activation_heights.nu5().map(BlockHeight::from_u32),
                NetworkUpgrade::Nu6 => activation_heights.nu6().map(BlockHeight::from_u32),
                NetworkUpgrade::Nu6_1 => activation_heights.nu6_1().map(BlockHeight::from_u32),
                NetworkUpgrade::Nu6_2 => activation_heights.nu6_2().map(BlockHeight::from_u32),
            },
        }
    }
}

/// Invalid chain type.
#[derive(thiserror::Error, Debug)]
#[error("Invalid chain type '{0}'. Expected one of: 'mainnet', 'testnet' or 'regtest'.")]
pub struct InvalidChainType(String);
