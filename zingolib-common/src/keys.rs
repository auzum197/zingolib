//! Wallet key material: the unified key store, the ids that name addresses derived from it,
//! and the transparent scope shared with the sync engine.

use std::io::{self, Read, Write};

use bip0039::Mnemonic;
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};

use zcash_address::unified::{Encoding as _, Ufvk};
use zcash_client_backend::address::UnifiedAddress;
use zcash_client_backend::keys::{Era, UnifiedSpendingKey};
use zcash_encoding::CompactSize;
use zcash_keys::keys::{DerivationError, UnifiedFullViewingKey};
use zcash_protocol::consensus::{NetworkConstants, Parameters};
use zcash_transparent::address::TransparentAddress;
use zcash_transparent::keys::{IncomingViewingKey, NonHardenedChildIndex, TransparentKeyScope};
use zip32::{AccountId, DiversifierIndex};

use crate::chain::ChainType;
use crate::serialization::ReadableWriteable;

/// Wallet-file tag for a key store without keys.
pub const KEY_TYPE_EMPTY: u8 = 0;
/// Wallet-file tag for a view-only key store.
pub const KEY_TYPE_VIEW: u8 = 1;
/// Wallet-file tag for a spending key store.
pub const KEY_TYPE_SPEND: u8 = 2;

/// Unique ID for unified addresses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct UnifiedAddressId {
    /// Account the address was derived for.
    pub account_id: AccountId,
    /// Index of the address within the account.
    pub address_index: u32,
}

impl UnifiedAddressId {
    /// Unversioned: the account id followed by the address index.
    pub fn read<R: Read>(mut reader: R) -> io::Result<Self> {
        let account_id = AccountId::try_from(reader.read_u32::<LittleEndian>()?).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("failed to read account id. {e}"),
            )
        })?;
        let address_index = reader.read_u32::<LittleEndian>()?;
        Ok(Self {
            account_id,
            address_index,
        })
    }

    /// Serialize into `writer`
    pub fn write<W: Write>(&self, mut writer: W) -> io::Result<()> {
        writer.write_u32::<LittleEndian>(self.account_id.into())?;
        writer.write_u32::<LittleEndian>(self.address_index)
    }
}

/// In-memory store for wallet spending or viewing keys
#[derive(Debug)]
pub enum UnifiedKeyStore {
    /// Wallet with spend capability
    Spend(Box<UnifiedSpendingKey>),
    /// Wallet with view capability
    View(Box<UnifiedFullViewingKey>),
    /// Wallet with no keys
    Empty,
}

impl UnifiedKeyStore {
    /// Create a unified key store from raw entropy (64-byte seed).
    pub fn new_from_seed(
        chain_type: ChainType,
        seed: &[u8; 64],
        account_index: zip32::AccountId,
    ) -> Result<Self, KeyError> {
        let usk = UnifiedSpendingKey::from_seed(&chain_type, seed, account_index)
            .map_err(KeyError::KeyDerivationError)?;

        Ok(UnifiedKeyStore::Spend(Box::new(usk)))
    }

    /// Create a unified key store from a mnemonic.
    ///
    /// Refer to BIP-0039 for details on seed generation from mnemonic phrases.
    pub fn new_from_mnemonic(
        chain_type: ChainType,
        mnemonic: &Mnemonic,
        account_index: zip32::AccountId,
    ) -> Result<Self, KeyError> {
        let seed = mnemonic.to_seed("");
        Self::new_from_seed(chain_type, &seed, account_index)
    }

    /// Create a unified key store from unified spending key bytes.
    pub fn new_from_usk(usk: &[u8]) -> Result<Self, KeyError> {
        let usk = UnifiedSpendingKey::from_bytes(Era::Orchard, usk)
            .map_err(|_| KeyError::KeyDecodingError)?;

        Ok(UnifiedKeyStore::Spend(Box::new(usk)))
    }

    /// Create a unified key store from unified full viewing key encoded string.
    pub fn new_from_ufvk(chain_type: ChainType, ufvk_encoded: String) -> Result<Self, KeyError> {
        if ufvk_encoded.starts_with(chain_type.hrp_sapling_extended_full_viewing_key()) {
            return Err(KeyError::InvalidFormat);
        }
        let (network_type, ufvk) =
            Ufvk::decode(&ufvk_encoded).map_err(|_| KeyError::KeyDecodingError)?;
        if network_type != chain_type.network_type() {
            return Err(KeyError::NetworkMismatch);
        }
        let ufvk = UnifiedFullViewingKey::parse(&ufvk).map_err(|_| KeyError::KeyDecodingError)?;

        Ok(UnifiedKeyStore::View(Box::new(ufvk)))
    }

    /// Returns true if [`UnifiedKeyStore`] is of `Spend` variant
    #[must_use]
    pub fn is_spending_key(&self) -> bool {
        matches!(self, UnifiedKeyStore::Spend(_))
    }

    /// Returns true if [`UnifiedKeyStore`] is of `Empty` variant
    #[must_use]
    pub fn is_empty(&self) -> bool {
        matches!(self, UnifiedKeyStore::Empty)
    }

    /// Returns the default receivers for unified address generation depending on the wallet's capability.
    /// Returns `None` if the wallet does not have viewing capabilities of at least 1 shielded pool.
    #[must_use]
    pub fn default_receivers(&self) -> Option<ReceiverSelection> {
        match self {
            UnifiedKeyStore::Spend(_) => Some(ReceiverSelection::orchard_only()),
            UnifiedKeyStore::View(ufvk) => {
                if ufvk.orchard().is_some() {
                    Some(ReceiverSelection::orchard_only())
                } else if ufvk.sapling().is_some() {
                    Some(ReceiverSelection::sapling_only())
                } else {
                    None
                }
            }
            UnifiedKeyStore::Empty => None,
        }
    }

    /// Generates a unified address for the given `unified_address_index` and `receivers`.
    pub fn generate_unified_address(
        &self,
        unified_address_index: u32,
        receivers: ReceiverSelection,
    ) -> Result<UnifiedAddress, KeyError> {
        let orchard_receiver = if receivers.orchard {
            let fvk = orchard::keys::FullViewingKey::try_from(self)?;
            Some(fvk.address_at(unified_address_index, orchard::keys::Scope::External))
        } else {
            None
        };

        let sapling_receiver = if receivers.sapling {
            Some(self.derive_sapling_address(unified_address_index)?)
        } else {
            None
        };

        let unified_address =
            UnifiedAddress::from_receivers(orchard_receiver, sapling_receiver, None)
                .ok_or(KeyError::UnifiedAddressError)?;

        Ok(unified_address)
    }

    /// Generates a transparent address for the given `address_index` and `scope`.
    pub fn generate_transparent_address(
        &self,
        address_index: NonHardenedChildIndex,
        scope: TransparentScope,
    ) -> Result<TransparentAddress, KeyError> {
        let account_pubkey = UnifiedFullViewingKey::try_from(self)?
            .transparent()
            .ok_or(KeyError::NoViewCapability)?
            .clone();

        let transparent_address = match scope {
            TransparentScope::External => account_pubkey
                .derive_external_ivk()?
                .derive_address(address_index)?,
            TransparentScope::Internal => account_pubkey
                .derive_internal_ivk()?
                .derive_address(address_index)?,
            TransparentScope::Refund => account_pubkey
                .derive_ephemeral_ivk()?
                .derive_ephemeral_address(address_index)?,
        };

        Ok(transparent_address)
    }

    fn derive_sapling_address(
        &self,
        unified_address_index: u32,
    ) -> Result<sapling_crypto::PaymentAddress, KeyError> {
        let fvk = sapling_crypto::zip32::DiversifiableFullViewingKey::try_from(self)?;
        let mut address;
        let mut diversifier_index = DiversifierIndex::new();
        let mut valid_diversifier_count = 0;

        // not all sapling diversifier indexes produce valid sapling diversifiers.
        // therefore, `diversifier_index` may be larger than `unified_address_index` as only the valid payment
        // addresses are counted.
        loop {
            (diversifier_index, address) = fvk
                .find_address(diversifier_index)
                .expect("diversifier index overflow");
            valid_diversifier_count += 1;
            if valid_diversifier_count - 1 == unified_address_index {
                break;
            }

            diversifier_index
                .increment()
                .expect("diversifier index overflow");
        }

        Ok(address)
    }

    /// Returns the number of valid sapling diversifiers when incrementing from 0 to `sapling_diversifier_index` inclusive.
    ///
    /// For example, if 10 is returned, the `sapling_diversifier_index` is associated with the 10th valid sapling
    /// diversifier when incrementing from a diversifier index of 0.
    pub fn determine_nth_valid_sapling_diversifier(
        &self,
        sapling_diversifier_index: DiversifierIndex,
    ) -> Result<u32, KeyError> {
        let fvk = sapling_crypto::zip32::DiversifiableFullViewingKey::try_from(self)?;
        let mut _address;
        let mut diversifier_index = DiversifierIndex::new();
        let mut valid_diversifier_count = 0;

        loop {
            (diversifier_index, _address) = fvk
                .find_address(diversifier_index)
                .expect("diversifier index overflow");
            valid_diversifier_count += 1;
            if diversifier_index == sapling_diversifier_index {
                break;
            }

            diversifier_index
                .increment()
                .expect("diversifier index overflow");
        }

        Ok(valid_diversifier_count)
    }
}

impl ReadableWriteable<ChainType, ChainType> for UnifiedKeyStore {
    const VERSION: u8 = 0;

    fn read<R: Read>(mut reader: R, input: ChainType) -> io::Result<Self> {
        let _version = Self::get_version(&mut reader)?;
        let key_type = reader.read_u8()?;
        Ok(match key_type {
            KEY_TYPE_SPEND => UnifiedKeyStore::Spend(Box::new(read_usk(reader)?)),
            KEY_TYPE_VIEW => {
                UnifiedKeyStore::View(Box::new(UnifiedFullViewingKey::read(reader, input)?))
            }
            KEY_TYPE_EMPTY => UnifiedKeyStore::Empty,
            x => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Unknown key type: {x}"),
                ));
            }
        })
    }

    fn write<W: Write>(&self, mut writer: W, input: ChainType) -> io::Result<()> {
        writer.write_u8(Self::VERSION)?;
        match self {
            UnifiedKeyStore::Spend(usk) => {
                writer.write_u8(KEY_TYPE_SPEND)?;
                write_usk(usk, &mut writer)
            }
            UnifiedKeyStore::View(ufvk) => {
                writer.write_u8(KEY_TYPE_VIEW)?;
                ufvk.write(&mut writer, input)
            }
            UnifiedKeyStore::Empty => writer.write_u8(KEY_TYPE_EMPTY),
        }
    }
}
fn read_usk<R: Read>(mut reader: R) -> io::Result<UnifiedSpendingKey> {
    let len = CompactSize::read(&mut reader)?;
    let mut usk = vec![0u8; len as usize];
    reader.read_exact(&mut usk)?;

    UnifiedSpendingKey::from_bytes(Era::Orchard, &usk)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "USK bytes are invalid"))
}

fn write_usk<W: Write>(usk: &UnifiedSpendingKey, mut writer: W) -> io::Result<()> {
    let usk_bytes = usk.to_bytes(Era::Orchard);
    CompactSize::write(&mut writer, usk_bytes.len())?;
    writer.write_all(&usk_bytes)
}

impl ReadableWriteable<ChainType, ChainType> for UnifiedFullViewingKey {
    const VERSION: u8 = 0;

    fn read<R: Read>(mut reader: R, input: ChainType) -> io::Result<Self> {
        let len = CompactSize::read(&mut reader)?;
        let mut ufvk = vec![0u8; len as usize];
        reader.read_exact(&mut ufvk)?;
        let ufvk_encoded = std::str::from_utf8(&ufvk)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;

        UnifiedFullViewingKey::decode(&input, ufvk_encoded).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("UFVK decoding error: {e}"),
            )
        })
    }

    fn write<W: Write>(&self, mut writer: W, input: ChainType) -> io::Result<()> {
        let ufvk_bytes = self.encode(&input).as_bytes().to_vec();
        CompactSize::write(&mut writer, ufvk_bytes.len())?;
        writer.write_all(&ufvk_bytes)?;
        Ok(())
    }
}

impl TryFrom<&UnifiedKeyStore> for UnifiedSpendingKey {
    type Error = KeyError;
    fn try_from(unified_key_store: &UnifiedKeyStore) -> Result<Self, Self::Error> {
        match unified_key_store {
            UnifiedKeyStore::Spend(usk) => Ok(*usk.clone()),
            _ => Err(KeyError::NoSpendCapability),
        }
    }
}
impl TryFrom<&UnifiedKeyStore> for orchard::keys::SpendingKey {
    type Error = KeyError;
    fn try_from(unified_key_store: &UnifiedKeyStore) -> Result<Self, Self::Error> {
        let usk = UnifiedSpendingKey::try_from(unified_key_store)?;
        Ok(*usk.orchard())
    }
}
impl TryFrom<&UnifiedKeyStore> for sapling_crypto::zip32::ExtendedSpendingKey {
    type Error = KeyError;
    fn try_from(unified_key_store: &UnifiedKeyStore) -> Result<Self, Self::Error> {
        let usk = UnifiedSpendingKey::try_from(unified_key_store)?;
        Ok(usk.sapling().clone())
    }
}
impl TryFrom<&UnifiedKeyStore> for zcash_transparent::keys::AccountPrivKey {
    type Error = KeyError;
    fn try_from(unified_key_store: &UnifiedKeyStore) -> Result<Self, Self::Error> {
        let usk = UnifiedSpendingKey::try_from(unified_key_store)?;
        Ok(usk.transparent().clone())
    }
}

impl TryFrom<&UnifiedKeyStore> for UnifiedFullViewingKey {
    type Error = KeyError;
    fn try_from(unified_key_store: &UnifiedKeyStore) -> Result<Self, Self::Error> {
        match unified_key_store {
            UnifiedKeyStore::Spend(usk) => Ok(usk.to_unified_full_viewing_key()),
            UnifiedKeyStore::View(ufvk) => Ok(*ufvk.clone()),
            UnifiedKeyStore::Empty => Err(KeyError::NoViewCapability),
        }
    }
}
impl TryFrom<&UnifiedKeyStore> for orchard::keys::FullViewingKey {
    type Error = KeyError;
    fn try_from(unified_key_store: &UnifiedKeyStore) -> Result<Self, Self::Error> {
        let ufvk = UnifiedFullViewingKey::try_from(unified_key_store)?;
        ufvk.orchard().ok_or(KeyError::NoViewCapability).cloned()
    }
}
impl TryFrom<&UnifiedKeyStore> for sapling_crypto::zip32::DiversifiableFullViewingKey {
    type Error = KeyError;
    fn try_from(unified_key_store: &UnifiedKeyStore) -> Result<Self, Self::Error> {
        let ufvk = UnifiedFullViewingKey::try_from(unified_key_store)?;
        ufvk.sapling().ok_or(KeyError::NoViewCapability).cloned()
    }
}
impl TryFrom<&UnifiedKeyStore> for zcash_transparent::keys::AccountPubKey {
    type Error = KeyError;
    fn try_from(unified_key_store: &UnifiedKeyStore) -> Result<Self, Self::Error> {
        let ufvk = UnifiedFullViewingKey::try_from(unified_key_store)?;
        ufvk.transparent()
            .ok_or(KeyError::NoViewCapability)
            .cloned()
    }
}

/// Selects the receivers for the creation of a new unified address.
#[derive(Debug, Clone, Copy, PartialEq, Default, serde::Deserialize, serde::Serialize)]
pub struct ReceiverSelection {
    /// Orchard
    pub orchard: bool,
    /// Sapling
    pub sapling: bool,
}

impl ReceiverSelection {
    /// All shielded receivers.
    #[must_use]
    pub fn all_shielded() -> Self {
        Self {
            orchard: true,
            sapling: true,
        }
    }

    /// Only orchard receiver.
    #[must_use]
    pub fn orchard_only() -> Self {
        Self {
            orchard: true,
            sapling: false,
        }
    }

    /// Only sapling receiver.
    #[must_use]
    pub fn sapling_only() -> Self {
        Self {
            orchard: false,
            sapling: true,
        }
    }
}

impl ReadableWriteable for ReceiverSelection {
    const VERSION: u8 = 2;

    fn read<R: Read>(mut reader: R, _input: ()) -> io::Result<Self> {
        let _version = Self::get_version(&mut reader)?;
        let receivers = reader.read_u8()?;
        Ok(Self {
            orchard: receivers & 0b1 != 0,
            sapling: receivers & 0b10 != 0,
        })
    }

    fn write<W: Write>(&self, mut writer: W, _input: ()) -> io::Result<()> {
        writer.write_u8(Self::VERSION)?;
        let mut receivers = 0;
        if self.orchard {
            receivers |= 0b1;
        }
        if self.sapling {
            receivers |= 0b10;
        }
        writer.write_u8(receivers)?;
        Ok(())
    }
}

#[test]
fn read_write_receiver_selections() {
    for (i, receivers_selected) in (0..4)
        .map(|n| ReceiverSelection::read([2, n].as_slice(), ()).unwrap())
        .enumerate()
    {
        let mut receivers_selected_bytes = [0; 2];
        receivers_selected
            .write(receivers_selected_bytes.as_mut_slice(), ())
            .unwrap();
        assert_eq!(i as u8, receivers_selected_bytes[1]);
    }
}

/// Child index for the `change` path level in the BIP44 hierarchy (a.k.a. scope/chain).
#[derive(Debug, Clone, Copy, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub enum TransparentScope {
    /// External scope
    External,
    /// Internal scope (a.k.a. change)
    Internal,
    /// Refund scope (a.k.a. ephemeral)
    Refund,
}

impl std::fmt::Display for TransparentScope {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                TransparentScope::External => "external",
                TransparentScope::Internal => "internal",
                TransparentScope::Refund => "refund",
            }
        )
    }
}

impl From<TransparentScope> for TransparentKeyScope {
    fn from(value: TransparentScope) -> Self {
        match value {
            TransparentScope::External => TransparentKeyScope::EXTERNAL,
            TransparentScope::Internal => TransparentKeyScope::INTERNAL,
            TransparentScope::Refund => TransparentKeyScope::EPHEMERAL,
        }
    }
}

impl TryFrom<u8> for TransparentScope {
    type Error = std::io::Error;

    fn try_from(value: u8) -> std::io::Result<Self> {
        match value {
            0 => Ok(TransparentScope::External),
            1 => Ok(TransparentScope::Internal),
            2 => Ok(TransparentScope::Refund),
            _ => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "invalid scope value",
            )),
        }
    }
}

/// Errors associated with key and address derivation
// TODO: make error private as contains external crate types. have public API safe higher level error type i.e. WalletError.
#[derive(Debug, thiserror::Error)]
pub enum KeyError {
    /// Error associated with standard IO
    #[error("{0}")]
    IoError(#[from] std::io::Error),
    /// Invalid account ID
    #[error("Account ID should be at most 31 bits")]
    InvalidAccountId(#[from] zip32::TryFromIntError),
    /// Invalid account ID
    #[error("No keys found for the given account id. Try adding the account.")]
    NoAccountKeys,
    /// Key derivation failed
    #[error("Key derivation failed")]
    KeyDerivationError(#[from] DerivationError),
    /// Key decoding failed
    #[error("Key decoding failed")]
    KeyDecodingError,
    /// Key parsing failed
    #[error("Key parsing failed. {0}")]
    KeyParseError(#[from] zcash_address::unified::ParseError),
    /// No spend capability
    #[error("No spend capability")]
    NoSpendCapability,
    /// No view capability
    #[error("No view capability")]
    NoViewCapability,
    /// Invalid non-hardened child indexes
    #[error("Outside range of non-hardened child indexes")]
    InvalidNonHardenedChildIndex,
    /// Network mismatch
    #[error("Decoded unified full viewing key does not match current network")]
    NetworkMismatch,
    /// Invalid format
    #[error("Viewing keys must be imported in the unified format")]
    InvalidFormat,
    /// Unified address missing shielded receiver
    #[error("Unified address must contain a shielded receiver")]
    UnifiedAddressError,
    /// Transparent address generation failed. Latest transparent address has not received funds.
    #[error(
        "Transparent address generation failed. Latest transparent address has not received funds."
    )]
    GapError,
    /// Invalid mnemonic phrase.
    #[error("Invalid mnemonic phrase: {0}")]
    InvalidMnemonicPhrase(#[from] bip0039::Error),
}

impl From<bip32::Error> for KeyError {
    fn from(value: bip32::Error) -> Self {
        Self::KeyDerivationError(DerivationError::Transparent(value))
    }
}
