//! Wallet key material: the unified key store, the ids that name addresses derived from it,
//! and the transparent scope shared with the sync engine.

use std::io::{self, Read, Write};

use bip0039::Mnemonic;
use byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};

use zcash_address::unified::{Container as _, Encoding as _, Fvk, Ufvk};
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
/// Reads a `CompactSize`-prefixed byte string. `take` grows the buffer only as bytes arrive, so a
/// corrupt length allocates no more than the input actually holds.
fn read_compact_bytes<R: Read>(mut reader: R, name: &str) -> io::Result<Vec<u8>> {
    let len = CompactSize::read(&mut reader)?;
    let mut bytes = Vec::new();
    reader.take(len).read_to_end(&mut bytes)?;
    if bytes.len() as u64 != len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{name} length {len} runs past the end of the input"),
        ));
    }
    Ok(bytes)
}

/// Unified encoding typecode of a Sapling item.
const SAPLING_TYPECODE: u64 = 2;
/// Where `ask` sits in a Sapling extended spending key: after the depth, parent key tag, child
/// index and chain code.
const SAPLING_ASK: std::ops::Range<usize> = 41..73;

/// sapling-crypto parses `ask` through `CtOption::and_then`, which runs its closure even for a
/// non-canonical scalar, and that closure panics on one. So the Sapling item of the key is found
/// and its `ask` checked before `UnifiedSpendingKey::from_bytes` sees it. Anything else malformed
/// is left for `from_bytes` to reject.
fn check_sapling_ask(usk: &[u8]) -> io::Result<()> {
    // The leading four bytes are the era, which `from_bytes` checks.
    let mut items = usk.get(4..).unwrap_or_default();
    loop {
        let (Ok(typecode), Ok(len)) = (
            CompactSize::read(&mut items),
            CompactSize::read_t::<_, usize>(&mut items),
        ) else {
            return Ok(());
        };
        let Some((item, rest)) = items.split_at_checked(len) else {
            return Ok(());
        };
        if typecode == SAPLING_TYPECODE {
            let ask = item
                .get(SAPLING_ASK)
                .and_then(|ask| <[u8; 32]>::try_from(ask).ok());
            // The canonical encoding of zero is all zero bytes, and a Sapling `ask` is never zero.
            return match ask {
                Some(ask)
                    if ask == [0; 32] || bool::from(jubjub::Fr::from_bytes(&ask).is_none()) =>
                {
                    Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "sapling spend authorizing key is zero or not a canonical scalar",
                    ))
                }
                _ => Ok(()),
            };
        }
        items = rest;
    }
}

/// sapling-crypto parses `ak` through `CtOption::and_then`, which runs its closure even for bytes
/// that are not a curve point, and that closure panics on them. So the Sapling item of the key is
/// found and its `ak` checked before `UnifiedFullViewingKey::decode` sees it. Anything else
/// malformed is left for `decode` to reject.
fn check_sapling_ak(ufvk_encoded: &str) -> io::Result<()> {
    let Ok((_, ufvk)) = Ufvk::decode(ufvk_encoded) else {
        return Ok(());
    };
    let not_a_point = ufvk.items().iter().any(|item| match item {
        Fvk::Sapling(fvk) => <[u8; 32]>::try_from(&fvk[..32])
            .is_ok_and(|ak| bool::from(jubjub::AffinePoint::from_bytes(ak).is_none())),
        _ => false,
    });
    if not_a_point {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "sapling spend validating key is not a curve point",
        ))
    } else {
        Ok(())
    }
}

fn read_usk<R: Read>(reader: R) -> io::Result<UnifiedSpendingKey> {
    let usk = read_compact_bytes(reader, "unified spending key")?;
    check_sapling_ask(&usk)?;

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

    fn read<R: Read>(reader: R, input: ChainType) -> io::Result<Self> {
        let ufvk = read_compact_bytes(reader, "unified full viewing key")?;
        let ufvk_encoded = std::str::from_utf8(&ufvk)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
        check_sapling_ak(ufvk_encoded)?;

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
        let version = Self::get_version(&mut reader)?;
        if version < Self::VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("receiver selection version {version} is no longer readable"),
            ));
        }
        let receivers = reader.read_u8()?;
        if receivers & !0b11 != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown receiver selection bits {receivers:#b}"),
            ));
        }
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

#[test]
fn receiver_selection_rejects_unknown_bits_and_old_versions() {
    assert!(ReceiverSelection::read([2, 0b100].as_slice(), ()).is_err());
    assert!(ReceiverSelection::read([1, 0b01].as_slice(), ()).is_err());
}

#[test]
fn key_store_length_past_the_input_is_an_error_not_an_allocation() {
    // Tag 254 prefixes a four-byte CompactSize, here 0x02000000: the largest length it allows.
    let huge_length = [254, 0, 0, 0, 2];
    for key_type in [KEY_TYPE_SPEND, KEY_TYPE_VIEW] {
        let bytes = [&[0, key_type][..], &huge_length, b"short"].concat();
        assert!(UnifiedKeyStore::read(bytes.as_slice(), ChainType::Mainnet).is_err());
    }
}

#[cfg(test)]
fn test_usk() -> UnifiedSpendingKey {
    UnifiedSpendingKey::from_seed(&ChainType::Mainnet, &[7; 32], AccountId::ZERO).unwrap()
}

#[cfg(test)]
fn key_store_bytes(key_type: u8, key: &[u8]) -> Vec<u8> {
    let mut out = vec![0, key_type];
    CompactSize::write(&mut out, key.len()).unwrap();
    out.extend_from_slice(key);
    out
}

#[test]
fn spending_key_store_round_trips() {
    let bytes = key_store_bytes(KEY_TYPE_SPEND, &test_usk().to_bytes(Era::Orchard));
    let read = UnifiedKeyStore::read(bytes.as_slice(), ChainType::Mainnet).unwrap();
    let mut written = Vec::new();
    read.write(&mut written, ChainType::Mainnet).unwrap();
    assert_eq!(written, bytes);
}

#[test]
fn malformed_sapling_ask_is_an_error_not_a_panic() {
    let usk = test_usk();
    let valid = usk.to_bytes(Era::Orchard);
    let sapling = usk.sapling().to_bytes();
    let sapling_at = valid
        .windows(sapling.len())
        .position(|window| window == sapling)
        .unwrap();
    let ask_at = sapling_at + SAPLING_ASK.start;
    for ask in [[0xff; 32], [0; 32]] {
        let mut malformed = valid.clone();
        malformed[ask_at..ask_at + 32].copy_from_slice(&ask);
        let bytes = key_store_bytes(KEY_TYPE_SPEND, &malformed);
        assert!(UnifiedKeyStore::read(bytes.as_slice(), ChainType::Mainnet).is_err());
    }
}

#[test]
fn sapling_ak_off_the_curve_is_an_error_not_a_panic() {
    let valid = test_usk()
        .to_unified_full_viewing_key()
        .encode(&ChainType::Mainnet);
    let bytes = key_store_bytes(KEY_TYPE_VIEW, valid.as_bytes());
    assert!(UnifiedKeyStore::read(bytes.as_slice(), ChainType::Mainnet).is_ok());

    let (network, ufvk) = Ufvk::decode(&valid).unwrap();
    let items = ufvk
        .items()
        .into_iter()
        .map(|item| match item {
            Fvk::Sapling(mut fvk) => {
                fvk[..32].copy_from_slice(&[0xff; 32]);
                Fvk::Sapling(fvk)
            }
            other => other,
        })
        .collect();
    let malformed = Ufvk::try_from_items(items).unwrap().encode(&network);
    let bytes = key_store_bytes(KEY_TYPE_VIEW, malformed.as_bytes());
    assert!(UnifiedKeyStore::read(bytes.as_slice(), ChainType::Mainnet).is_err());
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
