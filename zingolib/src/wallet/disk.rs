//! Reading and writing a [`LightWallet`] through the wallet file layout in
//! `zingolib_file_format`.
//!
//! ## Encrypted wallets
//!
//! Build an encrypted wallet, save it, and reload it. The passphrase is a plain `String`, so
//! callers don't depend on the `secrecy` crate.
//!
//! ```no_run
//! use std::io::Cursor;
//!
//! use zingolib::config::{ChainType, WalletConfig};
//! use zingolib::wallet::LightWallet;
//! use zingolib::wallet::encryption::{self, EncryptionConfig};
//!
//! # fn demo(chain_type: ChainType, wallet_config: WalletConfig) -> Result<(), Box<dyn std::error::Error>> {
//! let passphrase = "a strong passphrase".to_string();
//!
//! // Encryption is a construction input. Argon2id runs once during construction, and the
//! // derived key is cached so each later save only pays for the fast symmetric step. For a
//! // constrained device, use `EncryptionConfig::with_params(passphrase,
//! // encryption::Argon2Params::with_memory_mib(32))` instead.
//! let mut wallet = LightWallet::new(
//!     chain_type,
//!     wallet_config,
//!     Some(EncryptionConfig::new(passphrase.clone())),
//! )?;
//!
//! // `save` returns an encrypted envelope, or `None` when nothing changed since the last save.
//! // Persist these bytes wherever the wallet file lives.
//! let Some(encrypted) = wallet.save()? else { return Ok(()) };
//! assert!(encryption::is_encrypted(&encrypted));
//!
//! // Later, reload from those bytes with the same passphrase. A wrong or missing passphrase
//! // returns an error rather than a corrupt wallet.
//! let reloaded =
//!     LightWallet::read_encrypted(Cursor::new(&encrypted), chain_type, Some(passphrase))?;
//! assert!(reloaded.is_encrypted());
//! # Ok(())
//! # }
//! ```
//!
//! To encrypt an already-open wallet or rotate the passphrase, use
//! [`LightWallet::set_passphrase`].

use std::{
    collections::BTreeMap,
    io::{self, Error, ErrorKind, Read, Write},
};

use zcash_protocol::consensus;
use zip32::AccountId;

use zingolib_common::{
    chain::ChainType,
    keys::{KeyError, ReceiverSelection},
};
pub use zingolib_file_format::{WalletFile, WalletFileRef, first_addresses};

use super::LightWallet;
use pepper_sync::{keys::transparent, wallet::KeyIdInterface};

impl LightWallet {
    /// Changes in version 41:
    /// `ChainType` serialized as u8 instead of string to decouple from fmt::Display and reduce bytes stored.
    #[must_use]
    pub const fn serialized_version() -> u64 {
        WalletFile::VERSION
    }

    /// Serialize into `writer`
    pub fn write<W: Write>(
        &self,
        writer: W,
        consensus_parameters: &impl consensus::Parameters,
    ) -> io::Result<()> {
        WalletFileRef {
            chain_type: self.chain_type,
            mnemonic: self.mnemonic.as_ref(),
            birthday: self.birthday,
            unified_key_store: &self.unified_key_store,
            unified_addresses: self
                .unified_addresses
                .iter()
                .map(|(address_id, address)| {
                    (
                        *address_id,
                        ReceiverSelection {
                            orchard: address.orchard().is_some(),
                            sapling: address.sapling().is_some(),
                        },
                    )
                })
                .collect(),
            transparent_addresses: self.transparent_addresses.keys().copied().collect(),
            wallet_blocks: &self.wallet_blocks,
            wallet_transactions: &self.wallet_transactions,
            nullifier_map: &self.nullifier_map,
            outpoint_map: &self.outpoint_map,
            shard_trees: &self.shard_trees,
            sync_state: &self.sync_state,
            wallet_settings: &self.wallet_settings,
            price_list: &self.price_list,
        }
        .write(writer, consensus_parameters)
    }

    /// Derives the wallet's addresses from the keys and address ids held in `file`.
    pub(crate) fn from_file(file: WalletFile) -> Result<Self, KeyError> {
        let WalletFile {
            read_version,
            chain_type,
            mnemonic,
            birthday,
            unified_key_store,
            unified_addresses,
            transparent_addresses,
            wallet_blocks,
            wallet_transactions,
            nullifier_map,
            outpoint_map,
            shard_trees,
            sync_state,
            wallet_settings,
            price_list,
        } = file;

        let key_store = |account_id: AccountId| {
            unified_key_store
                .get(&account_id)
                .ok_or(KeyError::NoAccountKeys)
        };

        let unified_addresses = unified_addresses
            .into_iter()
            .map(|(address_id, receivers)| {
                let address = key_store(address_id.account_id)?
                    .generate_unified_address(address_id.address_index, receivers)?;
                Ok((address_id, address))
            })
            .collect::<Result<BTreeMap<_, _>, KeyError>>()?;

        let mut encoded_transparent_addresses = BTreeMap::new();
        for address_id in transparent_addresses {
            match key_store(address_id.account_id())?
                .generate_transparent_address(address_id.address_index(), address_id.scope())
            {
                Ok(address) => {
                    encoded_transparent_addresses.insert(
                        address_id,
                        transparent::encode_address(&chain_type, address),
                    );
                }
                Err(KeyError::NoViewCapability) => (),
                Err(e) => return Err(e),
            }
        }

        Ok(Self {
            current_version: Self::serialized_version(),
            read_version,
            chain_type,
            mnemonic,
            birthday,
            unified_key_store,
            unified_addresses,
            transparent_addresses: encoded_transparent_addresses,
            wallet_blocks,
            wallet_transactions,
            nullifier_map,
            outpoint_map,
            shard_trees,
            sync_state,
            wallet_settings,
            price_list,
            #[cfg(any(test, feature = "testutils"))]
            send_proposal: None,
            save_required: false,
            encryption: None,
        })
    }

    /// Deserialize into `reader`
    /// Read a wallet, transparently decrypting it first if the file is an encrypted envelope.
    ///
    /// If the file begins with the encryption magic, `passphrase` is required and is used to
    /// derive the key and decrypt the payload. The derived key is cached on the returned
    /// wallet so subsequent saves re-encrypt with the same passphrase. Otherwise the file is
    /// read as a plaintext wallet and `passphrase` is ignored.
    pub fn read_encrypted<R: Read>(
        reader: R,
        chain_type: ChainType,
        passphrase: Option<String>,
    ) -> io::Result<Self> {
        let (file, session) = WalletFile::read_encrypted(reader, chain_type, passphrase)?;
        let mut wallet =
            Self::from_file(file).map_err(|e| Error::new(ErrorKind::InvalidData, e))?;
        wallet.encryption = session;
        Ok(wallet)
    }

    // TODO: update to return WalletError
    pub fn read<R: Read>(reader: R, chain_type: ChainType) -> io::Result<Self> {
        Self::from_file(WalletFile::read(reader, chain_type)?)
            .map_err(|e| Error::new(ErrorKind::InvalidData, e))
    }
}

#[cfg(any(test, feature = "testutils"))]
pub mod testing;
