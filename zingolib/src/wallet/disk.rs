//! This mod contains write and read functionality of impl `LightWallet`

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    io::{self, Error, ErrorKind, Read, Write},
    num::NonZeroU32,
};

use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use log::info;

use bip0039::Mnemonic;
use zip32::AccountId;

use zcash_encoding::{Optional, Vector};
use zcash_keys::keys::UnifiedSpendingKey;
use zcash_primitives::transaction::TxId;
use zcash_protocol::consensus::{self, BlockHeight};
use zcash_transparent::keys::NonHardenedChildIndex;

use zingo_common_components::protocol::ActivationHeights;
use zingo_netutils::lightwallet_protocol::TreeState;
use zingolib_common::serialization::ReadableWriteable;
use zingolib_price::PriceList;

use secrecy::SecretString;

use super::encryption;
use super::keys::unified::{ReceiverSelection, UnifiedAddressId};
use super::{LightWallet, error::KeyError};
use crate::wallet::legacy::WalletOptions;
use crate::wallet::{WalletSettings, legacy::WalletZecPriceInfo, utils};
use crate::{
    config::ChainType,
    wallet::{
        keys::{legacy::WalletCapability, unified::UnifiedKeyStore},
        legacy::{BlockData, TxMap},
    },
};
use pepper_sync::{
    config::{PerformanceLevel, SyncConfig, TransparentAddressDiscovery},
    keys::transparent::{self, TransparentAddressId, TransparentScope},
    wallet::{
        KeyIdInterface, NullifierMap, OutputId, ScanTarget, ShardTrees, SyncState, WalletBlock,
        WalletTransaction,
    },
};

/// Decoded contents of a wallet file. Every field is public so the wallet can be
/// assembled from it without the file layer knowing anything about [`LightWallet`].
pub struct WalletFile {
    /// Layout version the file was read with.
    pub read_version: u64,
    /// Network the wallet belongs to.
    pub chain_type: ChainType,
    /// Seed phrase, absent for wallets built from keys.
    pub mnemonic: Option<Mnemonic>,
    /// First block that can hold wallet activity.
    pub birthday: BlockHeight,
    /// Keys per account.
    pub unified_key_store: BTreeMap<AccountId, UnifiedKeyStore>,
    /// Receiver selection per unified address, from which the address is re-derived.
    pub unified_addresses: BTreeMap<UnifiedAddressId, ReceiverSelection>,
    /// Derivation paths of the transparent addresses in use.
    pub transparent_addresses: BTreeSet<TransparentAddressId>,
    /// Blocks with wallet activity.
    pub wallet_blocks: BTreeMap<BlockHeight, WalletBlock>,
    /// Transactions with wallet activity.
    pub wallet_transactions: HashMap<TxId, WalletTransaction>,
    /// Nullifiers of wallet notes.
    pub nullifier_map: NullifierMap,
    /// Outpoints awaiting targeted scanning.
    pub outpoint_map: BTreeMap<OutputId, ScanTarget>,
    /// Note commitment trees.
    pub shard_trees: ShardTrees,
    /// Scan progress.
    pub sync_state: SyncState,
    /// Sync and confirmation settings.
    pub wallet_settings: WalletSettings,
    /// Cached ZEC prices.
    pub price_list: PriceList,
}

/// Borrowed view of what goes into a wallet file, so a wallet can be written without
/// cloning its collections.
pub struct WalletFileRef<'a> {
    /// Network the wallet belongs to.
    pub chain_type: ChainType,
    /// Seed phrase, absent for wallets built from keys.
    pub mnemonic: Option<&'a Mnemonic>,
    /// First block that can hold wallet activity.
    pub birthday: BlockHeight,
    /// Keys per account.
    pub unified_key_store: &'a BTreeMap<AccountId, UnifiedKeyStore>,
    /// Receiver selection per unified address.
    pub unified_addresses: BTreeMap<UnifiedAddressId, ReceiverSelection>,
    /// Derivation paths of the transparent addresses in use.
    pub transparent_addresses: BTreeSet<TransparentAddressId>,
    /// Blocks with wallet activity.
    pub wallet_blocks: &'a BTreeMap<BlockHeight, WalletBlock>,
    /// Transactions with wallet activity.
    pub wallet_transactions: &'a HashMap<TxId, WalletTransaction>,
    /// Nullifiers of wallet notes.
    pub nullifier_map: &'a NullifierMap,
    /// Outpoints awaiting targeted scanning.
    pub outpoint_map: &'a BTreeMap<OutputId, ScanTarget>,
    /// Note commitment trees.
    pub shard_trees: &'a ShardTrees,
    /// Scan progress.
    pub sync_state: &'a SyncState,
    /// Sync and confirmation settings.
    pub wallet_settings: &'a WalletSettings,
    /// Cached ZEC prices.
    pub price_list: &'a PriceList,
}

/// The address set every wallet starts with: the default unified address of account 0
/// and its first external transparent address. Address derivation later drops the
/// transparent entry when the keys cannot view transparent funds.
pub(crate) fn first_addresses(
    unified_key: &UnifiedKeyStore,
) -> (
    BTreeMap<UnifiedAddressId, ReceiverSelection>,
    BTreeSet<TransparentAddressId>,
) {
    let mut unified_addresses = BTreeMap::new();
    if let Some(receivers) = unified_key.default_receivers() {
        unified_addresses.insert(
            UnifiedAddressId {
                account_id: AccountId::ZERO,
                address_index: 0,
            },
            receivers,
        );
    }
    let transparent_addresses = BTreeSet::from([TransparentAddressId::new(
        AccountId::ZERO,
        TransparentScope::External,
        NonHardenedChildIndex::ZERO,
    )]);
    (unified_addresses, transparent_addresses)
}

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
        mut reader: R,
        chain_type: ChainType,
        passphrase: Option<String>,
    ) -> io::Result<Self> {
        let mut head = [0u8; 8];
        reader.read_exact(&mut head)?;

        if encryption::is_encrypted(&head) {
            let passphrase = passphrase.ok_or_else(|| {
                io::Error::new(
                    ErrorKind::InvalidInput,
                    encryption::WalletEncryptionError::PassphraseRequired.to_string(),
                )
            })?;
            let passphrase = SecretString::new(passphrase);
            let mut envelope = head.to_vec();
            reader.read_to_end(&mut envelope)?;
            let (plaintext, session) = encryption::decrypt(&passphrase, &envelope)
                .map_err(|e| io::Error::new(ErrorKind::InvalidData, e.to_string()))?;
            let mut wallet = Self::read(io::Cursor::new(plaintext.as_slice()), chain_type)?;
            wallet.encryption = Some(session);
            Ok(wallet)
        } else {
            // Not encrypted: replay the 8 bytes we peeked and read as a plaintext wallet.
            Self::read(io::Cursor::new(head).chain(reader), chain_type)
        }
    }

    // TODO: update to return WalletError
    pub fn read<R: Read>(reader: R, chain_type: ChainType) -> io::Result<Self> {
        Self::from_file(WalletFile::read(reader, chain_type)?)
            .map_err(|e| Error::new(ErrorKind::InvalidData, e))
    }
}

impl WalletFile {
    /// Changes in version 41:
    /// `ChainType` serialized as u8 instead of string to decouple from fmt::Display and reduce bytes stored.
    pub const VERSION: u64 = 41;

    /// Deserialize into `reader`
    pub fn read<R: Read>(mut reader: R, chain_type: ChainType) -> io::Result<Self> {
        let version = reader.read_u64::<LittleEndian>()?;
        info!("Reading wallet version {version}");
        match version {
            encryption::MAGIC_AS_VERSION => Err(io::Error::new(
                ErrorKind::InvalidInput,
                "wallet file is encrypted; load it with a passphrase via read_encrypted",
            )),
            ..32 => Self::read_v0(reader, chain_type, version),
            32..=41 => Self::read_v32(reader, chain_type, version),
            _ => Err(io::Error::new(
                ErrorKind::InvalidData,
                format!(
                    "Failed to read wallet version {}. Do you have the latest version?\n{}",
                    version, "Note: wallet files from zecwallet or beta zingo are not compatible"
                ),
            )),
        }
    }

    fn read_v0<R: Read>(mut reader: R, chain_type: ChainType, version: u64) -> io::Result<Self> {
        let mut wallet_capability = WalletCapability::read(&mut reader, chain_type)?;
        let mut _blocks = Vector::read(&mut reader, |r| BlockData::read(r))?;
        let transactions = if version <= 14 {
            TxMap::read_old(&mut reader, &wallet_capability)?
        } else {
            TxMap::read(&mut reader, &wallet_capability)?
        };

        let saved_network = match utils::read_string(&mut reader)?.as_str() {
            "main" => "mainnet",
            "test" => "testnet",
            "regtest" => "regtest",
            other => {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    format!("invalid chain type stored in wallet file: {}", other,),
                ));
            }
        };
        if saved_network != chain_type.to_string() {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!("wallet chain name {saved_network} doesn't match expected {chain_type}"),
            ));
        }

        let _wallet_options = if version <= 23 {
            WalletOptions::default()
        } else {
            WalletOptions::read(&mut reader)?
        };
        let birthday = BlockHeight::from_u32(
            reader
                .read_u64::<LittleEndian>()?
                .try_into()
                .expect("should never overflow"),
        );

        if version <= 22 {
            let _sapling_tree_verified = if version <= 12 {
                true
            } else {
                reader.read_u8()? == 1
            };
        }
        let _verified_tree = if version <= 21 {
            None
        } else {
            Optional::read(&mut reader, |r| {
                use prost::Message;

                let buf = Vector::read(r, byteorder::ReadBytesExt::read_u8)?;
                TreeState::decode(&buf[..])
                    .map_err(|e| io::Error::new(ErrorKind::InvalidData, e.to_string()))
            })?
        };

        let _price = if version <= 13 {
            WalletZecPriceInfo::default()
        } else {
            WalletZecPriceInfo::read(&mut reader)?
        };

        let _orchard_anchor_height_pairs = if version == 25 {
            Vector::read(&mut reader, |r| {
                let mut anchor_bytes = [0; 32];
                r.read_exact(&mut anchor_bytes)?;
                let block_height = BlockHeight::from_u32(r.read_u32::<LittleEndian>()?);
                Ok((
                    Option::<orchard::Anchor>::from(orchard::Anchor::from_bytes(anchor_bytes))
                        .ok_or(Error::new(ErrorKind::InvalidData, "Bad orchard anchor"))?,
                    block_height,
                ))
            })?
        } else {
            Vec::new()
        };

        let seed_bytes = Vector::read(&mut reader, byteorder::ReadBytesExt::read_u8)?;
        let mnemonic = if seed_bytes.is_empty() {
            None
        } else {
            let _account_index = if version >= 28 {
                reader.read_u32::<LittleEndian>()?
            } else {
                0
            };
            Some(
                Mnemonic::from_entropy(seed_bytes)
                    .map_err(|e| Error::new(ErrorKind::InvalidData, e.to_string()))?,
            )
        };

        // Derive unified spending key from seed and override temporary USK if wallet is pre v29.
        //
        // UnifiedSpendingKey is initially incomplete for old wallet versions.
        // This is due to the legacy transparent extended private key (ExtendedPrivKey) not containing all information required for BIP0032.
        // There is also the issue that the legacy transparent private key is derived an extra level to the external scope.
        if version < 29 {
            if let Some(mnemonic) = mnemonic.as_ref() {
                wallet_capability.unified_key_store = UnifiedKeyStore::Spend(Box::new(
                    UnifiedSpendingKey::from_seed(
                        &chain_type,
                        &mnemonic.to_seed(""),
                        AccountId::ZERO,
                    )
                    .map_err(|e| {
                        Error::new(
                            ErrorKind::InvalidData,
                            format!(
                                "failed to derive unified spending key from stored seed bytes. {e}"
                            ),
                        )
                    })?,
                ));
            } else if let UnifiedKeyStore::Spend(_) = &wallet_capability.unified_key_store {
                return Err(io::Error::other(
                    "loading from legacy spending keys with no seed to recover",
                ));
            }
        }

        let (unified_addresses, transparent_addresses) =
            first_addresses(&wallet_capability.unified_key_store);
        let mut unified_key_store = BTreeMap::new();
        unified_key_store.insert(zip32::AccountId::ZERO, wallet_capability.unified_key_store);

        // setup targetted scanning from zingo 1.x transaction data
        let mut sync_state = SyncState::new();
        pepper_sync::add_scan_targets(
            &mut sync_state,
            &transactions
                .transaction_records_by_id
                .0
                .values()
                .filter_map(|transaction| {
                    transaction
                        .status
                        .get_confirmed_height()
                        .map(|height| ScanTarget {
                            block_height: height,
                            txid: transaction.txid,
                            narrow_scan_area: true,
                        })
                })
                .collect::<Vec<_>>(),
        );

        Ok(Self {
            read_version: version,
            chain_type,
            mnemonic,
            birthday,
            unified_key_store,
            unified_addresses,
            transparent_addresses,
            wallet_blocks: BTreeMap::new(),
            wallet_transactions: HashMap::new(),
            nullifier_map: NullifierMap::new(),
            outpoint_map: BTreeMap::new(),
            shard_trees: ShardTrees::new(),
            sync_state,
            wallet_settings: WalletSettings {
                sync_config: SyncConfig {
                    transparent_address_discovery: TransparentAddressDiscovery::minimal(),
                    performance_level: PerformanceLevel::High,
                    ..SyncConfig::default()
                },
                min_confirmations: NonZeroU32::try_from(3).unwrap(),
            },
            price_list: PriceList::new(),
        })
    }

    fn read_v32<R: Read>(mut reader: R, chain_type: ChainType, version: u64) -> io::Result<Self> {
        if version >= 41 {
            let saved_network = match reader.read_u8()? {
                0 => ChainType::Mainnet,
                1 => ChainType::Testnet,
                2 => ChainType::Regtest(ActivationHeights::default()),
                other => {
                    return Err(Error::new(
                        ErrorKind::InvalidData,
                        format!("invalid chain type index stored in wallet file: {}", other,),
                    ));
                }
            };
            if saved_network.to_string() != chain_type.to_string() {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    format!(
                        "wallet chain name {saved_network} doesn't match expected {chain_type}"
                    ),
                ));
            }
        } else {
            let saved_network = match utils::read_string(&mut reader)?.as_str() {
                "main" => "mainnet",
                "test" => "testnet",
                "regtest" => "regtest",
                other => {
                    return Err(Error::new(
                        ErrorKind::InvalidData,
                        format!("invalid chain type stored in wallet file: {}", other,),
                    ));
                }
            };
            if saved_network != chain_type.to_string() {
                return Err(Error::new(
                    ErrorKind::InvalidData,
                    format!(
                        "wallet chain name {saved_network} doesn't match expected {chain_type}"
                    ),
                ));
            }
        }

        let seed_bytes = Vector::read(&mut reader, byteorder::ReadBytesExt::read_u8)?;
        let mnemonic = if seed_bytes.is_empty() {
            None
        } else {
            if version < 35 {
                let _account_index = reader.read_u32::<LittleEndian>()?;
            }
            Some(
                <Mnemonic>::from_entropy(seed_bytes)
                    .map_err(|e| Error::new(ErrorKind::InvalidData, e.to_string()))?,
            )
        };
        let birthday = BlockHeight::from_u32(reader.read_u32::<LittleEndian>()?);

        let unified_key_store = if version >= 35 {
            Vector::read(&mut reader, |r| {
                Ok((
                    zip32::AccountId::try_from(r.read_u32::<LittleEndian>()?)
                        .expect("only valid account ids are stored"),
                    UnifiedKeyStore::read(r, chain_type)?,
                ))
            })?
            .into_iter()
            .collect::<BTreeMap<_, _>>()
        } else {
            let mut keys = BTreeMap::new();
            keys.insert(
                zip32::AccountId::ZERO,
                UnifiedKeyStore::read(&mut reader, chain_type)?,
            );
            keys
        };

        let mut unified_addresses = Vector::read(&mut reader, |r| {
            let account_id = zip32::AccountId::try_from(r.read_u32::<LittleEndian>()?)
                .expect("only valid account ids are stored");
            let address_index = r.read_u32::<LittleEndian>()?;
            let receivers = ReceiverSelection::read(r, ())?;

            Ok((
                UnifiedAddressId {
                    account_id,
                    address_index,
                },
                receivers,
            ))
        })?
        .into_iter()
        .collect::<BTreeMap<_, _>>();
        let mut transparent_addresses =
            Vector::read(&mut reader, |r| TransparentAddressId::read(r))?
                .into_iter()
                .collect::<BTreeSet<_>>();

        // reset zingo 2.0 test version addresses
        if version < 36 {
            (unified_addresses, transparent_addresses) = first_addresses(
                unified_key_store
                    .get(&zip32::AccountId::ZERO)
                    .expect("account 0 must exist"),
            );
        }

        let wallet_blocks = Vector::read(&mut reader, |r| WalletBlock::read(r, ()))?
            .into_iter()
            .map(|block| (block.block_height(), block))
            .collect::<BTreeMap<_, _>>();
        let wallet_transactions =
            Vector::read(&mut reader, |r| WalletTransaction::read(r, &chain_type))?
                .into_iter()
                .map(|transaction| (transaction.txid(), transaction))
                .collect::<HashMap<_, _>>();
        let nullifier_map = NullifierMap::read(&mut reader, ())?;
        let outpoint_map = Vector::read(&mut reader, |mut r| {
            let output_id = if version >= 40 {
                OutputId::read(&mut r)?
            } else {
                OutputId::new(
                    TxId::read(&mut r)?,
                    u32::from(r.read_u16::<LittleEndian>()?),
                )
            };
            let scan_target = if version >= 37 {
                ScanTarget::read(r, ())?
            } else {
                let block_height = BlockHeight::from_u32(r.read_u32::<LittleEndian>()?);
                let txid = TxId::read(&mut r)?;

                ScanTarget {
                    block_height,
                    txid,
                    narrow_scan_area: true,
                }
            };

            Ok((output_id, scan_target))
        })?
        .into_iter()
        .collect::<BTreeMap<_, _>>();
        let shard_trees = ShardTrees::read(&mut reader, ())?;
        let sync_state = SyncState::read(&mut reader, ())?;

        let wallet_settings = if version >= 33 {
            WalletSettings {
                sync_config: SyncConfig::read(&mut reader, ())?,
                min_confirmations: if version >= 38 {
                    NonZeroU32::try_from(reader.read_u32::<LittleEndian>()?)
                        .expect("only valid non-zero u32s stored")
                } else {
                    NonZeroU32::try_from(3).expect("hard-coded non-zero integer")
                },
            }
        } else {
            WalletSettings {
                sync_config: SyncConfig {
                    transparent_address_discovery: TransparentAddressDiscovery::minimal(),
                    performance_level: PerformanceLevel::High,
                    ..SyncConfig::default()
                },
                min_confirmations: NonZeroU32::try_from(3).unwrap(),
            }
        };

        let price_list = if version >= 34 {
            PriceList::read(&mut reader, ())?
        } else {
            PriceList::new()
        };

        Ok(Self {
            read_version: version,
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
        })
    }
}

#[cfg(any(test, feature = "testutils"))]
pub mod testing;

impl WalletFileRef<'_> {
    /// Serialize into `writer`
    pub fn write<W: Write>(
        &self,
        mut writer: W,
        consensus_parameters: &impl consensus::Parameters,
    ) -> io::Result<()> {
        writer.write_u64::<LittleEndian>(WalletFile::VERSION)?;
        writer.write_u8(match self.chain_type {
            ChainType::Mainnet => 0,
            ChainType::Testnet => 1,
            ChainType::Regtest(_) => 2,
        })?;
        let seed_bytes = match self.mnemonic {
            Some(m) => m.clone().into_entropy(),
            None => vec![],
        };
        Vector::write(&mut writer, &seed_bytes, |w, byte| w.write_u8(*byte))?;
        writer.write_u32::<LittleEndian>(self.birthday.into())?;
        Vector::write(
            &mut writer,
            &self.unified_key_store.iter().collect::<Vec<_>>(),
            |w, (account_id, unified_key)| {
                w.write_u32::<LittleEndian>(u32::from(**account_id))?;
                unified_key.write(w, self.chain_type)
            },
        )?;
        // TODO: also store receiver selections in encoded memos.
        Vector::write(
            &mut writer,
            &self.unified_addresses.iter().collect::<Vec<_>>(),
            |w, (address_id, receivers)| {
                w.write_u32::<LittleEndian>(address_id.account_id.into())?;
                w.write_u32::<LittleEndian>(address_id.address_index)?;
                receivers.write(w, ())
            },
        )?;
        Vector::write(
            &mut writer,
            &self.transparent_addresses.iter().collect::<Vec<_>>(),
            |w, address_id| address_id.write(w),
        )?;
        Vector::write(
            &mut writer,
            &self.wallet_blocks.values().collect::<Vec<_>>(),
            |w, &block| block.write(w, ()),
        )?;
        Vector::write(
            &mut writer,
            &self.wallet_transactions.values().collect::<Vec<_>>(),
            |w, &transaction| transaction.write(w, consensus_parameters),
        )?;
        self.nullifier_map.write(&mut writer, ())?;
        Vector::write(
            &mut writer,
            &self.outpoint_map.iter().collect::<Vec<_>>(),
            |w, &(&output_id, &scan_target)| {
                output_id.write(&mut *w)?;
                scan_target.write(w, ())
            },
        )?;
        self.shard_trees.write(&mut writer, ())?;
        self.sync_state.write(&mut writer, ())?;
        self.wallet_settings.sync_config.write(&mut writer, ())?;
        writer.write_u32::<LittleEndian>(self.wallet_settings.min_confirmations.into())?;
        self.price_list.write(&mut writer, ())
    }
}
