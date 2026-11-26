//! Module for types associated with the zcash protocol and consensus.

use core::cmp::Ordering;
use core::fmt;
use core::ops::{Add, Sub};
use std::io::{self, Read, Write};

/// A wrapper type representing blockchain heights.
///
/// Safe conversion from various integer types, as well as addition and subtraction, are
/// provided.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BlockHeight(u32);

/// The height of the genesis block on a network.
pub const H0: BlockHeight = BlockHeight(0);

impl BlockHeight {
    /// Constructs [`BlockHeight`] from `u32`.
    pub const fn from_u32(v: u32) -> BlockHeight {
        BlockHeight(v)
    }

    /// Subtracts the provided value from this height, returning [`H0`] if this would result in
    /// underflow of the wrapped `u32`.
    pub fn saturating_sub(self, v: u32) -> BlockHeight {
        BlockHeight(self.0.saturating_sub(v))
    }
}

impl fmt::Display for BlockHeight {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl Ord for BlockHeight {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.cmp(&other.0)
    }
}

impl PartialOrd for BlockHeight {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl From<u32> for BlockHeight {
    fn from(value: u32) -> Self {
        BlockHeight(value)
    }
}

impl From<BlockHeight> for u32 {
    fn from(value: BlockHeight) -> u32 {
        value.0
    }
}

impl TryFrom<u64> for BlockHeight {
    type Error = core::num::TryFromIntError;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        u32::try_from(value).map(BlockHeight)
    }
}

impl From<BlockHeight> for u64 {
    fn from(value: BlockHeight) -> u64 {
        value.0 as u64
    }
}

impl TryFrom<i32> for BlockHeight {
    type Error = core::num::TryFromIntError;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        u32::try_from(value).map(BlockHeight)
    }
}

impl TryFrom<i64> for BlockHeight {
    type Error = core::num::TryFromIntError;

    fn try_from(value: i64) -> Result<Self, Self::Error> {
        u32::try_from(value).map(BlockHeight)
    }
}

impl From<BlockHeight> for i64 {
    fn from(value: BlockHeight) -> i64 {
        value.0 as i64
    }
}

impl Add<u32> for BlockHeight {
    type Output = Self;

    fn add(self, other: u32) -> Self {
        BlockHeight(self.0.saturating_add(other))
    }
}

impl Sub<u32> for BlockHeight {
    type Output = Self;

    fn sub(self, other: u32) -> Self {
        BlockHeight(self.0.saturating_sub(other))
    }
}

impl Sub<BlockHeight> for BlockHeight {
    type Output = u32;

    fn sub(self, other: BlockHeight) -> u32 {
        self.0.saturating_sub(other.0)
    }
}

/// The identifier for a Zcash transaction.
///
/// - For v1-4 transactions, this is a double-SHA-256 hash of the encoded transaction.
///   This means that it is malleable, and only a reliable identifier for transactions
///   that have been mined.
/// - For v5 transactions onwards, this identifier is derived only from "effecting" data,
///   and is non-malleable in all contexts.
#[derive(Clone, Copy, PartialOrd, Ord, PartialEq, Eq, Hash)]
pub struct TxId([u8; 32]);

impl fmt::Debug for TxId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The (byte-flipped) hex string is more useful than the raw bytes, because we can
        // look that up in RPC methods and block explorers.
        let txid_str = self.to_string();
        f.debug_tuple("TxId").field(&txid_str).finish()
    }
}

impl fmt::Display for TxId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut data = self.0;
        data.reverse();
        formatter.write_str(&hex::encode(data))
    }
}

impl AsRef<[u8; 32]> for TxId {
    fn as_ref(&self) -> &[u8; 32] {
        &self.0
    }
}

impl From<TxId> for [u8; 32] {
    fn from(value: TxId) -> Self {
        value.0
    }
}

impl TxId {
    /// The all-zeros txid. This is reserved as the txid of the transparent input to a coinbase
    /// transaction.
    pub const NULL: TxId = TxId([0u8; 32]);

    /// Wraps the given byte array as a TxId value
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        TxId(bytes)
    }

    /// Reads a 32-byte txid directly from the provided reader.
    pub fn read<R: Read>(mut reader: R) -> io::Result<Self> {
        let mut hash = [0u8; 32];
        reader.read_exact(&mut hash)?;
        Ok(TxId::from_bytes(hash))
    }

    /// Writes the 32-byte payload directly to the provided writer.
    pub fn write<W: Write>(&self, mut writer: W) -> io::Result<()> {
        writer.write_all(&self.0)?;
        Ok(())
    }

    /// Returns true when the txid consists of all zeros, indicating the input
    /// to a coinbase transaction.
    pub fn is_null(&self) -> bool {
        *self == Self::NULL
    }
}

/// Network types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkType {
    /// Mainnet
    Mainnet,
    /// Testnet
    Testnet,
    /// Regtest
    Regtest(ActivationHeights),
}

impl std::fmt::Display for NetworkType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let chain = match self {
            NetworkType::Mainnet => "mainnet",
            NetworkType::Testnet => "testnet",
            NetworkType::Regtest(_) => "regtest",
        };
        write!(f, "{chain}")
    }
}

/// Network upgrade activation heights for custom testnet and regtest network configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActivationHeights {
    overwinter: Option<u32>,
    sapling: Option<u32>,
    blossom: Option<u32>,
    heartwood: Option<u32>,
    canopy: Option<u32>,
    nu5: Option<u32>,
    nu6: Option<u32>,
    nu6_1: Option<u32>,
    nu6_2: Option<u32>,
    nu6_3: Option<u32>,
    nu7: Option<u32>,
}

impl Default for ActivationHeights {
    fn default() -> Self {
        Self::builder()
            .set_overwinter(Some(1))
            .set_sapling(Some(1))
            .set_blossom(Some(1))
            .set_heartwood(Some(1))
            .set_canopy(Some(1))
            .set_nu5(Some(1))
            .set_nu6(Some(1))
            .set_nu6_1(Some(1))
            .set_nu6_2(Some(1))
            .set_nu6_3(Some(1))
            .set_nu7(None)
            .build()
    }
}

impl ActivationHeights {
    /// Constructs new builder.
    pub fn builder() -> ActivationHeightsBuilder {
        ActivationHeightsBuilder::new()
    }

    /// Returns overwinter network upgrade activation height.
    pub fn overwinter(&self) -> Option<u32> {
        self.overwinter
    }

    /// Returns sapling network upgrade activation height.
    pub fn sapling(&self) -> Option<u32> {
        self.sapling
    }

    /// Returns blossom network upgrade activation height.
    pub fn blossom(&self) -> Option<u32> {
        self.blossom
    }

    /// Returns heartwood network upgrade activation height.
    pub fn heartwood(&self) -> Option<u32> {
        self.heartwood
    }

    /// Returns canopy network upgrade activation height.
    pub fn canopy(&self) -> Option<u32> {
        self.canopy
    }

    /// Returns nu5 network upgrade activation height.
    pub fn nu5(&self) -> Option<u32> {
        self.nu5
    }

    /// Returns nu6 network upgrade activation height.
    pub fn nu6(&self) -> Option<u32> {
        self.nu6
    }

    /// Returns nu6.1 network upgrade activation height.
    pub fn nu6_1(&self) -> Option<u32> {
        self.nu6_1
    }

    /// Returns nu6.2 network upgrade activation height.
    pub fn nu6_2(&self) -> Option<u32> {
        self.nu6_2
    }

    /// Returns nu6.3 network upgrade activation height.
    pub fn nu6_3(&self) -> Option<u32> {
        self.nu6_3
    }

    /// Returns nu7 network upgrade activation height.
    pub fn nu7(&self) -> Option<u32> {
        self.nu7
    }
}

/// Leverages a builder method to avoid new network upgrades from causing breaking changes to the public API.
pub struct ActivationHeightsBuilder {
    overwinter: Option<u32>,
    sapling: Option<u32>,
    blossom: Option<u32>,
    heartwood: Option<u32>,
    canopy: Option<u32>,
    nu5: Option<u32>,
    nu6: Option<u32>,
    nu6_1: Option<u32>,
    nu6_2: Option<u32>,
    nu6_3: Option<u32>,
    nu7: Option<u32>,
}

impl Default for ActivationHeightsBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ActivationHeightsBuilder {
    /// Constructs a builder with all fields set to `None`.
    pub fn new() -> Self {
        Self {
            overwinter: None,
            sapling: None,
            blossom: None,
            heartwood: None,
            canopy: None,
            nu5: None,
            nu6: None,
            nu6_1: None,
            nu6_2: None,
            nu6_3: None,
            nu7: None,
        }
    }

    /// Set `overwinter` field.
    pub fn set_overwinter(mut self, height: Option<u32>) -> Self {
        self.overwinter = height;

        self
    }

    /// Set `sapling` field.
    pub fn set_sapling(mut self, height: Option<u32>) -> Self {
        self.sapling = height;

        self
    }

    /// Set `blossom` field.
    pub fn set_blossom(mut self, height: Option<u32>) -> Self {
        self.blossom = height;

        self
    }

    /// Set `heartwood` field.
    pub fn set_heartwood(mut self, height: Option<u32>) -> Self {
        self.heartwood = height;

        self
    }

    /// Set `canopy` field.
    pub fn set_canopy(mut self, height: Option<u32>) -> Self {
        self.canopy = height;

        self
    }

    /// Set `nu5` field.
    pub fn set_nu5(mut self, height: Option<u32>) -> Self {
        self.nu5 = height;

        self
    }

    /// Set `nu6` field.
    pub fn set_nu6(mut self, height: Option<u32>) -> Self {
        self.nu6 = height;

        self
    }

    /// Set `nu6_1` field.
    pub fn set_nu6_1(mut self, height: Option<u32>) -> Self {
        self.nu6_1 = height;

        self
    }

    /// Set `nu6_2` field.
    pub fn set_nu6_2(mut self, height: Option<u32>) -> Self {
        self.nu6_2 = height;

        self
    }

    /// Set `nu6_3` field.
    pub fn set_nu6_3(mut self, height: Option<u32>) -> Self {
        self.nu6_3 = height;

        self
    }

    /// Set `nu7` field.
    pub fn set_nu7(mut self, height: Option<u32>) -> Self {
        self.nu7 = height;

        self
    }

    /// Builds `ActivationHeights` with assertions to ensure all earlier network upgrades are active with an activation
    /// height equal to or lower than the later network upgrades.
    pub fn build(self) -> ActivationHeights {
        if let Some(b) = self.sapling {
            assert!(self.overwinter.is_some_and(|a| a <= b));
        }
        if let Some(b) = self.blossom {
            assert!(self.sapling.is_some_and(|a| a <= b));
        }
        if let Some(b) = self.heartwood {
            assert!(self.blossom.is_some_and(|a| a <= b));
        }
        if let Some(b) = self.canopy {
            assert!(self.heartwood.is_some_and(|a| a <= b));
        }
        if let Some(b) = self.nu5 {
            assert!(self.canopy.is_some_and(|a| a <= b));
        }
        if let Some(b) = self.nu6 {
            assert!(self.nu5.is_some_and(|a| a <= b));
        }
        if let Some(b) = self.nu6_1 {
            assert!(self.nu6.is_some_and(|a| a <= b));
        }
        if let Some(b) = self.nu6_2 {
            assert!(self.nu6_1.is_some_and(|a| a <= b));
        }
        if let Some(b) = self.nu6_3 {
            assert!(self.nu6_2.is_some_and(|a| a <= b));
        }
        if let Some(b) = self.nu7 {
            assert!(
                self.nu6_3
                    .or(self.nu6_2)
                    .or(self.nu6_1)
                    .is_some_and(|a| a <= b)
            );
        }

        ActivationHeights {
            overwinter: self.overwinter,
            sapling: self.sapling,
            blossom: self.blossom,
            heartwood: self.heartwood,
            canopy: self.canopy,
            nu5: self.nu5,
            nu6: self.nu6,
            nu6_1: self.nu6_1,
            nu6_2: self.nu6_2,
            nu6_3: self.nu6_3,
            nu7: self.nu7,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ActivationHeights;

    #[test]
    fn activation_heights_preserve_nu6_2() {
        let heights = ActivationHeights::builder()
            .set_overwinter(Some(1))
            .set_sapling(Some(2))
            .set_blossom(Some(3))
            .set_heartwood(Some(4))
            .set_canopy(Some(5))
            .set_nu5(Some(6))
            .set_nu6(Some(7))
            .set_nu6_1(Some(8))
            .set_nu6_2(Some(9))
            .set_nu7(None)
            .build();

        assert_eq!(heights.nu6_2(), Some(9));
    }

    #[test]
    #[should_panic]
    fn activation_heights_reject_nu6_2_before_nu6_1() {
        let _ = ActivationHeights::builder()
            .set_nu6(Some(7))
            .set_nu6_1(Some(8))
            .set_nu6_2(Some(7))
            .build();
    }

    #[test]
    fn activation_heights_preserve_nu6_3() {
        let heights = ActivationHeights::builder()
            .set_overwinter(Some(1))
            .set_sapling(Some(2))
            .set_blossom(Some(3))
            .set_heartwood(Some(4))
            .set_canopy(Some(5))
            .set_nu5(Some(6))
            .set_nu6(Some(7))
            .set_nu6_1(Some(8))
            .set_nu6_2(Some(9))
            .set_nu6_3(Some(10))
            .set_nu7(None)
            .build();

        assert_eq!(heights.nu6_3(), Some(10));
    }

    #[test]
    #[should_panic]
    fn activation_heights_reject_nu6_3_before_nu6_2() {
        let _ = ActivationHeights::builder()
            .set_nu6(Some(7))
            .set_nu6_1(Some(8))
            .set_nu6_2(Some(9))
            .set_nu6_3(Some(8))
            .build();
    }
}
