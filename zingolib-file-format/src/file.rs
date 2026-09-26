//! The versioned wallet file container.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    io::{self, Error, ErrorKind, Read, Write},
};

use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};
use log::info;

use bip0039::Mnemonic;
use zip32::AccountId;

use zcash_encoding::Vector;
use zcash_primitives::transaction::TxId;
use zcash_protocol::consensus::{self, BlockHeight};

use secrecy::SecretString;
use zingolib_common::{
    chain::ChainType,
    keys::{ReceiverSelection, UnifiedAddressId, UnifiedKeyStore},
    serialization::ReadableWriteable,
};
use zingolib_price::PriceList;

use pepper_sync::{
    keys::transparent::TransparentAddressId,
    wallet::{
        NullifierMap, OutputId, ScanTarget, ShardTrees, SyncState, WalletBlock, WalletTransaction,
    },
};

use crate::encryption::{self, EncryptionSession};
use crate::settings::WalletSettings;

/// Decoded contents of a wallet file. Every field is public so a wallet can be assembled
/// from it without this crate knowing anything about the wallet type.
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

impl<'a> From<&'a WalletFile> for WalletFileRef<'a> {
    fn from(file: &'a WalletFile) -> Self {
        Self {
            chain_type: file.chain_type,
            mnemonic: file.mnemonic.as_ref(),
            birthday: file.birthday,
            unified_key_store: &file.unified_key_store,
            unified_addresses: file.unified_addresses.clone(),
            transparent_addresses: file.transparent_addresses.clone(),
            wallet_blocks: &file.wallet_blocks,
            wallet_transactions: &file.wallet_transactions,
            nullifier_map: &file.nullifier_map,
            outpoint_map: &file.outpoint_map,
            shard_trees: &file.shard_trees,
            sync_state: &file.sync_state,
            wallet_settings: &file.wallet_settings,
            price_list: &file.price_list,
        }
    }
}

impl WalletFile {
    /// The only layout accepted. The first pendrake-watch release wrote version 41, and no
    /// older layout was ever released, so nothing before it is readable.
    pub const VERSION: u64 = 41;

    /// Read a wallet file, transparently decrypting it first if it is an encrypted envelope.
    ///
    /// If the file begins with the encryption magic, `passphrase` is required and is used to
    /// derive the key and decrypt the payload. The session that opened the file is returned
    /// so later saves can re-encrypt with the same passphrase. Otherwise the file is read as
    /// a plaintext wallet, `passphrase` is ignored, and no session is returned.
    pub fn read_encrypted<R: Read>(
        reader: R,
        chain_type: ChainType,
        passphrase: Option<String>,
    ) -> io::Result<(Self, Option<EncryptionSession>)> {
        Self::open(reader, passphrase, |plaintext| {
            Self::read(plaintext, chain_type)
        })
    }

    /// [`Self::read_encrypted`] for a file whose chain is not known in advance. See
    /// [`Self::read_any`] for how the chain is recovered.
    pub fn read_encrypted_any<R: Read>(
        reader: R,
        passphrase: Option<String>,
    ) -> io::Result<(Self, Option<EncryptionSession>)> {
        Self::open(reader, passphrase, |plaintext| Self::read_any(plaintext))
    }

    fn open<R: Read, T>(
        mut reader: R,
        passphrase: Option<String>,
        decode: impl FnOnce(&mut dyn Read) -> io::Result<T>,
    ) -> io::Result<(T, Option<EncryptionSession>)> {
        let mut head = [0u8; 8];
        reader.read_exact(&mut head)?;

        if encryption::is_encrypted(&head) {
            let passphrase = passphrase.ok_or_else(|| {
                Error::new(
                    ErrorKind::InvalidInput,
                    encryption::WalletEncryptionError::PassphraseRequired.to_string(),
                )
            })?;
            let passphrase = SecretString::new(passphrase);
            let mut envelope = head.to_vec();
            reader.read_to_end(&mut envelope)?;
            let (plaintext, session) = encryption::decrypt(&passphrase, &envelope)
                .map_err(|e| Error::new(ErrorKind::InvalidData, e.to_string()))?;
            let decoded = decode(&mut io::Cursor::new(plaintext.as_slice()))?;
            Ok((decoded, Some(session)))
        } else {
            // Not encrypted: replay the 8 bytes we peeked and read as a plaintext wallet.
            let decoded = decode(&mut io::Cursor::new(head).chain(reader))?;
            Ok((decoded, None))
        }
    }

    /// Deserialize into `reader`, checking that the file belongs to `chain_type`.
    ///
    /// `chain_type` is also what the keys and transactions are decoded against, so a regtest
    /// wallet is read with the activation heights the caller configured.
    pub fn read<R: Read>(mut reader: R, chain_type: ChainType) -> io::Result<Self> {
        Self::read_version(&mut reader)?;
        let stored = ChainType::read(&mut reader)?;
        if stored.to_string() != chain_type.to_string() {
            return Err(Error::new(
                ErrorKind::InvalidData,
                format!("wallet chain name {stored} doesn't match expected {chain_type}"),
            ));
        }
        Self::read_body(reader, chain_type)
    }

    /// Deserialize into `reader`, taking the chain from the file itself.
    ///
    /// The file only stores the network name, so a regtest wallet comes back with the
    /// default activation heights.
    ///
    /// ```no_run
    /// use zingolib_file_format::WalletFile;
    ///
    /// let file = WalletFile::read_any(std::fs::File::open("zingo-wallet.dat")?)?;
    /// println!("{} wallet, layout version {}", file.chain_type, file.read_version);
    /// # Ok::<(), std::io::Error>(())
    /// ```
    pub fn read_any<R: Read>(mut reader: R) -> io::Result<Self> {
        Self::read_version(&mut reader)?;
        let chain_type = ChainType::read(&mut reader)?;
        Self::read_body(reader, chain_type)
    }

    fn read_version<R: Read>(mut reader: R) -> io::Result<()> {
        let version = reader.read_u64::<LittleEndian>()?;
        info!("Reading wallet version {version}");
        match version {
            Self::VERSION => Ok(()),
            encryption::MAGIC_AS_VERSION => Err(Error::new(
                ErrorKind::InvalidInput,
                "wallet file is encrypted; load it with a passphrase via read_encrypted",
            )),
            ..Self::VERSION => Err(Error::new(
                ErrorKind::InvalidData,
                format!(
                    "wallet layout version {version} predates the first release and cannot be read"
                ),
            )),
            _ => Err(Error::new(
                ErrorKind::InvalidData,
                format!("wallet layout version {version} is newer than this build supports"),
            )),
        }
    }

    fn read_body<R: Read>(mut reader: R, chain_type: ChainType) -> io::Result<Self> {
        let seed_bytes = Vector::read(&mut reader, byteorder::ReadBytesExt::read_u8)?;
        let mnemonic = if seed_bytes.is_empty() {
            None
        } else {
            Some(
                <Mnemonic>::from_entropy(seed_bytes)
                    .map_err(|e| Error::new(ErrorKind::InvalidData, e.to_string()))?,
            )
        };
        let birthday = BlockHeight::from_u32(reader.read_u32::<LittleEndian>()?);

        let unified_key_store = Vector::read(&mut reader, |r| {
            let account_id = AccountId::try_from(r.read_u32::<LittleEndian>()?)
                .map_err(|e| Error::new(ErrorKind::InvalidData, e.to_string()))?;
            Ok((account_id, UnifiedKeyStore::read(r, chain_type)?))
        })?
        .into_iter()
        .collect::<BTreeMap<_, _>>();

        let unified_addresses = Vector::read(&mut reader, |r| {
            Ok((
                UnifiedAddressId::read(&mut *r)?,
                ReceiverSelection::read(r, ())?,
            ))
        })?
        .into_iter()
        .collect::<BTreeMap<_, _>>();
        let transparent_addresses = Vector::read(&mut reader, |r| TransparentAddressId::read(r))?
            .into_iter()
            .collect::<BTreeSet<_>>();

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
            Ok((OutputId::read(&mut r)?, ScanTarget::read(r, ())?))
        })?
        .into_iter()
        .collect::<BTreeMap<_, _>>();
        let shard_trees = ShardTrees::read(&mut reader, ())?;
        let sync_state = SyncState::read(&mut reader, ())?;
        let wallet_settings = WalletSettings::read(&mut reader)?;
        let price_list = PriceList::read(&mut reader, ())?;

        Ok(Self {
            read_version: Self::VERSION,
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

impl WalletFileRef<'_> {
    /// Serialize into `writer`
    pub fn write<W: Write>(
        &self,
        mut writer: W,
        consensus_parameters: &impl consensus::Parameters,
    ) -> io::Result<()> {
        writer.write_u64::<LittleEndian>(WalletFile::VERSION)?;
        self.chain_type.write(&mut writer)?;
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
                address_id.write(&mut *w)?;
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
        self.wallet_settings.write(&mut writer)?;
        self.price_list.write(&mut writer, ())
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use zcash_keys::keys::UnifiedSpendingKey;
    use zcash_transparent::keys::NonHardenedChildIndex;
    use zingo_common_components::protocol::ActivationHeights;
    use zingolib_common::keys::TransparentScope;

    use crate::encryption::{Argon2Params, EncryptionConfig};

    use super::*;

    const PASSPHRASE: &str = "correct horse battery staple";

    fn fast_session(passphrase: &str) -> EncryptionSession {
        EncryptionConfig::with_params(
            passphrase.to_string(),
            Argon2Params {
                m_cost: 8,
                t_cost: 1,
                p_cost: 1,
            },
        )
        .derive()
        .unwrap()
    }

    fn mnemonic() -> Mnemonic {
        Mnemonic::from_entropy([7u8; 32]).unwrap()
    }

    fn fresh(chain_type: ChainType) -> WalletFile {
        let keys =
            UnifiedKeyStore::new_from_mnemonic(chain_type, &mnemonic(), AccountId::ZERO).unwrap();
        let mut unified_addresses = BTreeMap::new();
        unified_addresses.insert(
            UnifiedAddressId {
                account_id: AccountId::ZERO,
                address_index: 0,
            },
            keys.default_receivers().unwrap(),
        );
        let transparent_addresses = BTreeSet::from([TransparentAddressId::new(
            AccountId::ZERO,
            TransparentScope::External,
            NonHardenedChildIndex::ZERO,
        )]);
        WalletFile {
            read_version: WalletFile::VERSION,
            chain_type,
            mnemonic: Some(mnemonic()),
            birthday: BlockHeight::from_u32(2_000_000),
            unified_key_store: BTreeMap::from([(AccountId::ZERO, keys)]),
            unified_addresses,
            transparent_addresses,
            wallet_blocks: BTreeMap::new(),
            wallet_transactions: HashMap::new(),
            nullifier_map: NullifierMap::new(),
            outpoint_map: BTreeMap::new(),
            shard_trees: ShardTrees::new(),
            sync_state: SyncState::new(),
            wallet_settings: WalletSettings::default(),
            price_list: PriceList::new(),
        }
    }

    fn bytes(file: &WalletFile) -> Vec<u8> {
        let mut out = Vec::new();
        WalletFileRef::from(file)
            .write(&mut out, &file.chain_type)
            .unwrap();
        out
    }

    fn chains() -> [ChainType; 3] {
        [
            ChainType::Mainnet,
            ChainType::Testnet,
            ChainType::Regtest(ActivationHeights::default()),
        ]
    }

    fn error_text<T>(outcome: io::Result<T>) -> String {
        outcome.err().expect("expected an error").to_string()
    }

    #[test]
    fn write_is_deterministic() {
        for chain_type in chains() {
            let file = fresh(chain_type);
            assert_eq!(bytes(&file), bytes(&file));
        }
    }

    #[test]
    fn read_and_read_any_agree_on_every_chain() {
        for chain_type in chains() {
            let original = fresh(chain_type);
            let written = bytes(&original);

            let with_chain = WalletFile::read(written.as_slice(), chain_type).unwrap();
            let discovered = WalletFile::read_any(written.as_slice()).unwrap();

            for file in [&with_chain, &discovered] {
                assert_eq!(file.read_version, WalletFile::VERSION);
                assert_eq!(file.chain_type.to_string(), chain_type.to_string());
                assert_eq!(file.birthday, original.birthday);
                assert_eq!(
                    file.mnemonic.as_ref().map(|m| m.phrase().to_string()),
                    original.mnemonic.as_ref().map(|m| m.phrase().to_string())
                );
                assert_eq!(file.unified_addresses, original.unified_addresses);
                assert_eq!(file.transparent_addresses, original.transparent_addresses);
                assert_eq!(file.wallet_settings, original.wallet_settings);
                assert_eq!(bytes(file), written);
            }
        }
    }

    #[test]
    fn read_any_gives_regtest_default_heights_and_read_keeps_the_callers() {
        let custom = ChainType::Regtest(
            ActivationHeights::builder()
                .set_overwinter(Some(1))
                .set_sapling(Some(100))
                .set_blossom(Some(100))
                .set_heartwood(Some(100))
                .set_canopy(Some(100))
                .set_nu5(Some(100))
                .set_nu6(Some(100))
                .set_nu6_1(Some(100))
                .set_nu6_2(Some(100))
                .set_nu6_3(Some(100))
                .set_nu7(None)
                .build(),
        );
        let written = bytes(&fresh(custom));

        let discovered = WalletFile::read_any(written.as_slice()).unwrap();
        assert_eq!(
            discovered.chain_type,
            ChainType::Regtest(ActivationHeights::default())
        );

        let with_chain = WalletFile::read(written.as_slice(), custom).unwrap();
        assert_eq!(with_chain.chain_type, custom);
    }

    #[test]
    fn read_rejects_a_chain_mismatch() {
        let written = bytes(&fresh(ChainType::Testnet));
        let message = error_text(WalletFile::read(written.as_slice(), ChainType::Mainnet));
        assert!(
            message.contains("testnet doesn't match expected mainnet"),
            "{message}"
        );
        assert!(WalletFile::read_any(written.as_slice()).is_ok());
    }

    #[test]
    fn older_and_newer_layouts_are_rejected_by_both_readers() {
        let written = bytes(&fresh(ChainType::Mainnet));
        for (version, expected) in [(40u64, "predates"), (42u64, "newer")] {
            let mut patched = written.clone();
            patched[..8].copy_from_slice(&version.to_le_bytes());
            assert!(error_text(WalletFile::read_any(patched.as_slice())).contains(expected));
            assert!(
                error_text(WalletFile::read(patched.as_slice(), ChainType::Mainnet))
                    .contains(expected)
            );
        }
    }

    #[test]
    fn unknown_chain_tag_is_rejected() {
        let mut written = bytes(&fresh(ChainType::Mainnet));
        written[8] = 9;
        let message = error_text(WalletFile::read_any(written.as_slice()));
        assert!(message.contains("invalid chain type index"), "{message}");
    }

    #[test]
    fn plaintext_readers_refuse_an_envelope() {
        let envelope = fast_session(PASSPHRASE)
            .encrypt(&bytes(&fresh(ChainType::Mainnet)))
            .unwrap();
        assert!(error_text(WalletFile::read_any(envelope.as_slice())).contains("encrypted"));
        assert!(
            error_text(WalletFile::read(envelope.as_slice(), ChainType::Mainnet))
                .contains("encrypted")
        );
    }

    #[test]
    fn every_truncation_is_an_error_not_a_panic() {
        let written = bytes(&fresh(ChainType::Testnet));
        for len in 0..written.len() {
            assert!(
                WalletFile::read_any(&written[..len]).is_err(),
                "prefix of {len} bytes read successfully"
            );
        }
    }

    #[test]
    fn encrypted_round_trip_with_and_without_a_known_chain() {
        let original = fresh(ChainType::Testnet);
        let written = bytes(&original);
        let envelope = fast_session(PASSPHRASE).encrypt(&written).unwrap();
        assert_ne!(envelope, written);

        let (discovered, session) =
            WalletFile::read_encrypted_any(envelope.as_slice(), Some(PASSPHRASE.to_string()))
                .unwrap();
        assert!(session.is_some());
        assert_eq!(bytes(&discovered), written);

        let (with_chain, session) = WalletFile::read_encrypted(
            envelope.as_slice(),
            ChainType::Testnet,
            Some(PASSPHRASE.to_string()),
        )
        .unwrap();
        assert!(session.is_some());
        assert_eq!(bytes(&with_chain), written);

        let reencrypted = session.unwrap().encrypt(&written).unwrap();
        let (again, _) =
            WalletFile::read_encrypted_any(reencrypted.as_slice(), Some(PASSPHRASE.to_string()))
                .unwrap();
        assert_eq!(bytes(&again), written);
    }

    #[test]
    fn encrypted_readers_need_the_right_passphrase() {
        let envelope = fast_session(PASSPHRASE)
            .encrypt(&bytes(&fresh(ChainType::Mainnet)))
            .unwrap();

        let missing = error_text(WalletFile::read_encrypted_any(envelope.as_slice(), None));
        assert!(missing.to_lowercase().contains("passphrase"), "{missing}");

        assert!(
            WalletFile::read_encrypted_any(envelope.as_slice(), Some("wrong".to_string())).is_err()
        );
        assert!(
            WalletFile::read_encrypted(
                envelope.as_slice(),
                ChainType::Mainnet,
                Some("wrong".to_string())
            )
            .is_err()
        );
    }

    #[test]
    fn encrypted_reader_still_checks_the_chain() {
        let envelope = fast_session(PASSPHRASE)
            .encrypt(&bytes(&fresh(ChainType::Testnet)))
            .unwrap();
        let message = error_text(WalletFile::read_encrypted(
            envelope.as_slice(),
            ChainType::Mainnet,
            Some(PASSPHRASE.to_string()),
        ));
        assert!(message.contains("doesn't match"), "{message}");
    }

    #[test]
    fn plaintext_file_ignores_a_passphrase_and_yields_no_session() {
        let original = fresh(ChainType::Mainnet);
        let written = bytes(&original);
        for passphrase in [None, Some(PASSPHRASE.to_string())] {
            let (file, session) =
                WalletFile::read_encrypted_any(written.as_slice(), passphrase).unwrap();
            assert!(session.is_none());
            assert_eq!(bytes(&file), written);
        }
    }

    #[test]
    fn view_only_wallet_without_a_seed_round_trips() {
        let chain_type = ChainType::Mainnet;
        let spend = fresh(chain_type);
        let usk = UnifiedSpendingKey::try_from(&spend.unified_key_store[&AccountId::ZERO]).unwrap();
        let ufvk = usk.to_unified_full_viewing_key().encode(&chain_type);

        let mut view = fresh(chain_type);
        view.mnemonic = None;
        view.unified_key_store = BTreeMap::from([(
            AccountId::ZERO,
            UnifiedKeyStore::new_from_ufvk(chain_type, ufvk).unwrap(),
        )]);

        let written = bytes(&view);
        let read_back = WalletFile::read_any(written.as_slice()).unwrap();
        assert!(read_back.mnemonic.is_none());
        assert!(matches!(
            read_back.unified_key_store[&AccountId::ZERO],
            UnifiedKeyStore::View(_)
        ));
        assert_eq!(bytes(&read_back), written);
    }

    #[test]
    fn address_sets_and_settings_survive_a_round_trip() {
        let mut file = fresh(ChainType::Testnet);
        for index in 1..4 {
            file.unified_addresses.insert(
                UnifiedAddressId {
                    account_id: AccountId::ZERO,
                    address_index: index,
                },
                ReceiverSelection {
                    orchard: index % 2 == 0,
                    sapling: true,
                },
            );
            file.transparent_addresses.insert(TransparentAddressId::new(
                AccountId::ZERO,
                TransparentScope::Internal,
                NonHardenedChildIndex::from_index(index).unwrap(),
            ));
        }
        file.wallet_settings.min_confirmations = NonZeroU32::new(7).unwrap();

        let read_back = WalletFile::read_any(bytes(&file).as_slice()).unwrap();
        assert_eq!(read_back.unified_addresses, file.unified_addresses);
        assert_eq!(read_back.transparent_addresses, file.transparent_addresses);
        assert_eq!(read_back.wallet_settings, file.wallet_settings);
    }
}
