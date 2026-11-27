//! Module for configuration and construction of [`crate::wallet::LightWallet`].

use std::{collections::BTreeMap, num::NonZeroU32};

use bip0039::{English, Mnemonic};
use http::uri::InvalidUri;

use zcash_protocol::consensus::{BlockHeight, NetworkUpgrade, Parameters};

pub use ekdromos_common::chain::{ChainType, InvalidChainType};

use crate::wallet::{
    WalletBase, WalletSettings,
    error::{KeyError, WalletError},
    keys::unified::UnifiedKeyStore,
};

pub const DEFAULT_INDEXER_URI: &str = "https://zec.rocks:443";
pub const DEFAULT_INDEXER_URI_TESTNET: &str = "https://testnet.zec.rocks";

/// The first block that can hold activity of a wallet. Sync starts there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Birthday {
    /// 100 blocks below the chain tip of the indexer, for keys that have no history. Only
    /// [`crate::lightclient::LightClient::create`] can resolve it.
    ChainTip,
    Height(u32),
    /// The earliest birthday. Sync scans the whole shielded chain.
    SaplingActivation,
}

/// Configuration data for the construction of a [`crate::wallet::LightWallet`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WalletConfig {
    /// Generate a wallet with a new seed for a number of accounts.
    NewSeed {
        no_of_accounts: NonZeroU32,
        birthday: Birthday,
        wallet_settings: WalletSettings,
    },
    /// Generate a wallet from a mnemonic phrase for a number of accounts.
    MnemonicPhrase {
        mnemonic_phrase: String,
        no_of_accounts: NonZeroU32,
        birthday: Birthday,
        wallet_settings: WalletSettings,
    },
    /// Generate a wallet from an encoded unified full viewing key.
    // TODO: take concrete UFVK type
    Ufvk {
        ufvk: String,
        birthday: Birthday,
        wallet_settings: WalletSettings,
    },
    /// Generate a wallet from a unified spending key.
    // TODO: take concrete USK type
    Usk {
        usk: Vec<u8>,
        birthday: Birthday,
        wallet_settings: WalletSettings,
    },
}

impl Birthday {
    #[allow(clippy::result_large_err)]
    fn height(self, chain_type: ChainType) -> Result<BlockHeight, WalletError> {
        match self {
            Birthday::ChainTip => Err(WalletError::ChainTipUnknown),
            Birthday::Height(height) => Ok(BlockHeight::from_u32(height)),
            Birthday::SaplingActivation => Ok(chain_type
                .activation_height(NetworkUpgrade::Sapling)
                .expect("should have some sapling activation height")),
        }
    }
}

impl WalletConfig {
    pub(crate) fn birthday_mut(&mut self) -> &mut Birthday {
        match self {
            WalletConfig::NewSeed { birthday, .. }
            | WalletConfig::MnemonicPhrase { birthday, .. }
            | WalletConfig::Ufvk { birthday, .. }
            | WalletConfig::Usk { birthday, .. } => birthday,
        }
    }

    /// Resolves the wallet config into the base data needed to construct the wallet.
    ///
    /// `NewSeed` generates the wallet base data from a new 24-word mnemonic.
    #[allow(clippy::result_large_err)]
    pub(crate) fn resolve(self, chain_type: ChainType) -> Result<WalletBase, WalletError> {
        match self {
            WalletConfig::NewSeed {
                no_of_accounts,
                birthday,
                wallet_settings,
            } => WalletConfig::MnemonicPhrase {
                mnemonic_phrase: Mnemonic::<English>::generate(bip0039::Count::Words24)
                    .into_phrase(),
                no_of_accounts,
                birthday,
                wallet_settings,
            }
            .resolve(chain_type),
            WalletConfig::MnemonicPhrase {
                mnemonic_phrase: mnemonic,
                no_of_accounts,
                birthday,
                wallet_settings,
            } => {
                let mnemonic = Mnemonic::from_phrase(mnemonic)?;
                let no_of_accounts = u32::from(no_of_accounts);
                let unified_key_store = (0..no_of_accounts)
                    .map(|account_index| {
                        let account_id = zip32::AccountId::try_from(account_index)?;
                        Ok((
                            account_id,
                            UnifiedKeyStore::new_from_mnemonic(chain_type, &mnemonic, account_id)?,
                        ))
                    })
                    .collect::<Result<BTreeMap<_, _>, KeyError>>()?;
                Ok(WalletBase {
                    unified_key_store,
                    mnemonic: Some(mnemonic),
                    birthday: birthday.height(chain_type)?,
                    wallet_settings,
                    encryption: None,
                })
            }
            WalletConfig::Ufvk {
                ufvk,
                birthday,
                wallet_settings,
            } => {
                let mut unified_key_store = BTreeMap::new();
                unified_key_store.insert(
                    zip32::AccountId::ZERO,
                    UnifiedKeyStore::new_from_ufvk(chain_type, ufvk)?,
                );
                Ok(WalletBase {
                    unified_key_store,
                    mnemonic: None,
                    birthday: birthday.height(chain_type)?,
                    wallet_settings,
                    encryption: None,
                })
            }
            WalletConfig::Usk {
                usk,
                birthday,
                wallet_settings,
            } => {
                let mut unified_key_store = BTreeMap::new();
                unified_key_store.insert(
                    zip32::AccountId::ZERO,
                    UnifiedKeyStore::new_from_usk(usk.as_slice())?,
                );
                Ok(WalletBase {
                    unified_key_store,
                    mnemonic: None,
                    birthday: birthday.height(chain_type)?,
                    wallet_settings,
                    encryption: None,
                })
            }
        }
    }
}

/// Constructs a http::Uri from a `server` string. If `server` is `None` use the `DEFAULT_INDEXER_URI`.
/// If the provided string is missing the http prefix, a prefix of `http://` will be added.
/// If the provided string is missing a port, a port of `:9067` will be added.
pub fn construct_lightwalletd_uri(server: Option<String>) -> Result<http::Uri, InvalidUri> {
    match server {
        Some(s) => {
            if s.is_empty() {
                return Ok(http::Uri::default());
            } else {
                let mut s = if s.starts_with("http") {
                    s
                } else {
                    "http://".to_string() + &s
                };
                let uri: http::Uri = s.parse()?;
                if uri.port().is_none() {
                    s += ":9067";
                }
                s
            }
        }
        None => DEFAULT_INDEXER_URI.to_string(),
    }
    .parse()
}
