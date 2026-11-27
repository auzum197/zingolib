//! Chain data that a source returns.
//!
//! The types state the length and the structure of each value. A source converts its own
//! format into them and returns [`crate::SourceError::InvalidData`] for a value that does
//! not fit. The types do not prove that a value is a valid curve point or commitment.

use std::{error::Error, fmt};

use zcash_protocol::{TxId, consensus::BlockHeight};

/// A byte string with a length that its type does not accept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InvalidLength {
    expected: usize,
    actual: usize,
}

impl InvalidLength {
    /// The length that the type accepts.
    pub fn expected(&self) -> usize {
        self.expected
    }

    /// The length of the rejected byte string.
    pub fn actual(&self) -> usize {
        self.actual
    }
}

impl fmt::Display for InvalidLength {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "expected {} bytes, found {}", self.expected, self.actual)
    }
}

impl Error for InvalidLength {}

macro_rules! byte_array {
    ($(#[$doc:meta])* $name:ident, $length:literal, $order:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name([u8; $length]);

        impl $name {
            /// The number of bytes in a value.
            pub const LENGTH: usize = $length;

            /// Creates a value from its bytes.
            pub const fn from_bytes(bytes: [u8; $length]) -> Self {
                Self(bytes)
            }

            /// Returns the bytes.
            pub const fn as_bytes(&self) -> &[u8; $length] {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self([0; $length])
            }
        }

        impl From<[u8; $length]> for $name {
            fn from(bytes: [u8; $length]) -> Self {
                Self(bytes)
            }
        }

        impl From<$name> for [u8; $length] {
            fn from(value: $name) -> Self {
                value.0
            }
        }

        impl AsRef<[u8]> for $name {
            fn as_ref(&self) -> &[u8] {
                &self.0
            }
        }

        impl TryFrom<&[u8]> for $name {
            type Error = InvalidLength;

            fn try_from(bytes: &[u8]) -> Result<Self, Self::Error> {
                bytes.try_into().map(Self).map_err(|_| InvalidLength {
                    expected: $length,
                    actual: bytes.len(),
                })
            }
        }

        impl fmt::LowerHex for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                byte_array!(@hex $order, self.0, f)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt::LowerHex::fmt(self, f)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({:x})", stringify!($name), self)
            }
        }
    };
    (@hex forward, $bytes:expr, $f:expr) => {
        $bytes.iter().try_for_each(|byte| write!($f, "{byte:02x}"))
    };
    (@hex reversed, $bytes:expr, $f:expr) => {
        $bytes.iter().rev().try_for_each(|byte| write!($f, "{byte:02x}"))
    };
}

byte_array!(
    /// The hash of a block. It prints as hex in reversed byte order, as block explorers do.
    BlockHash, 32, reversed
);
byte_array!(
    /// The nullifier of a spent note.
    Nullifier, 32, forward
);
byte_array!(
    /// The commitment of a new note: `cmu` for Sapling, `cmx` for Orchard and Ironwood.
    NoteCommitment, 32, forward
);
byte_array!(
    /// The ephemeral public key of an output.
    EphemeralKey, 32, forward
);
byte_array!(
    /// A node of a note commitment tree.
    TreeNode, 32, forward
);
byte_array!(
    /// The first 52 bytes of a note ciphertext.
    CompactCiphertext, 52, forward
);

/// The height of a block and its hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BlockId {
    /// Block height.
    pub height: BlockHeight,
    /// Block hash, if the source supplies it.
    pub hash: Option<BlockHash>,
}

/// A block with only the data that payment detection needs, as in ZIP 307.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactBlock {
    /// Block height.
    pub height: BlockHeight,
    /// Block hash.
    pub hash: BlockHash,
    /// Hash of the previous block.
    pub prev_hash: BlockHash,
    /// Block time in seconds since the Unix epoch.
    pub time: u32,
    /// Transactions of the block.
    pub transactions: Vec<CompactTx>,
    /// Tree sizes at the end of the block, if the source supplies them.
    pub tree_sizes: Option<TreeSizes>,
}

/// The number of note commitments in each tree at the end of a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct TreeSizes {
    /// Sapling note commitments.
    pub sapling: u32,
    /// Orchard note commitments.
    pub orchard: u32,
    /// Ironwood note commitments.
    pub ironwood: u32,
}

/// A transaction with only the data that payment detection needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactTx {
    /// Position of the transaction in its block.
    pub index: u64,
    /// Transaction ID.
    pub txid: TxId,
    /// Transaction fee in zatoshis.
    pub fee: u32,
    /// Sapling spends.
    pub spends: Vec<CompactSaplingSpend>,
    /// Sapling outputs.
    pub outputs: Vec<CompactSaplingOutput>,
    /// Orchard actions.
    pub actions: Vec<CompactOrchardAction>,
    /// Ironwood actions.
    pub ironwood_actions: Vec<CompactOrchardAction>,
    /// Transparent inputs.
    pub transparent_inputs: Vec<CompactTxIn>,
    /// Transparent outputs.
    pub transparent_outputs: Vec<TxOut>,
}

impl CompactTx {
    /// A transaction with no spends and no outputs.
    pub fn new(index: u64, txid: TxId) -> Self {
        Self {
            index,
            txid,
            fee: 0,
            spends: Vec::new(),
            outputs: Vec::new(),
            actions: Vec::new(),
            ironwood_actions: Vec::new(),
            transparent_inputs: Vec::new(),
            transparent_outputs: Vec::new(),
        }
    }
}

/// A Sapling spend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct CompactSaplingSpend {
    /// Nullifier of the spent note.
    pub nullifier: Nullifier,
}

/// A Sapling output with a truncated ciphertext.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct CompactSaplingOutput {
    /// Note commitment.
    pub cmu: NoteCommitment,
    /// Ephemeral public key.
    pub ephemeral_key: EphemeralKey,
    /// Truncated note ciphertext.
    pub ciphertext: CompactCiphertext,
}

/// An Orchard or Ironwood action with a truncated ciphertext.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct CompactOrchardAction {
    /// Nullifier of the spent note.
    pub nullifier: Nullifier,
    /// Note commitment of the new note.
    pub cmx: NoteCommitment,
    /// Ephemeral public key.
    pub ephemeral_key: EphemeralKey,
    /// Truncated note ciphertext.
    pub ciphertext: CompactCiphertext,
}

/// The outpoint that a transparent input spends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CompactTxIn {
    /// ID of the transaction that holds the spent output.
    pub prevout_txid: TxId,
    /// Index of the spent output.
    pub prevout_index: u32,
}

/// A transparent output.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct TxOut {
    /// Value in zatoshis.
    pub value: u64,
    /// Locking script.
    pub script_pub_key: Vec<u8>,
}

/// A frontier with a number of ommers that does not fit its position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InvalidFrontier {
    expected: usize,
    actual: usize,
}

impl fmt::Display for InvalidFrontier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "expected {} ommers for the position, found {}",
            self.expected, self.actual
        )
    }
}

impl Error for InvalidFrontier {}

/// The last leaf of a note commitment tree, with the nodes that give the tree root.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Frontier {
    position: u64,
    leaf: TreeNode,
    ommers: Vec<TreeNode>,
}

impl Frontier {
    /// Creates a frontier.
    ///
    /// # Errors
    ///
    /// Fails if the number of ommers is not the number of one bits in `position`.
    pub fn new(
        position: u64,
        leaf: TreeNode,
        ommers: Vec<TreeNode>,
    ) -> Result<Self, InvalidFrontier> {
        let expected = position.count_ones() as usize;
        if ommers.len() == expected {
            Ok(Self {
                position,
                leaf,
                ommers,
            })
        } else {
            Err(InvalidFrontier {
                expected,
                actual: ommers.len(),
            })
        }
    }

    /// Position of the last leaf. The tree holds one more leaf than this number.
    pub fn position(&self) -> u64 {
        self.position
    }

    /// The last leaf.
    pub fn leaf(&self) -> TreeNode {
        self.leaf
    }

    /// The left siblings on the path from the leaf to the root, lowest first.
    pub fn ommers(&self) -> &[TreeNode] {
        &self.ommers
    }
}

/// The note commitment trees at the end of a block. `None` is an empty tree.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TreeState {
    /// Block height.
    pub height: BlockHeight,
    /// Block hash.
    pub hash: BlockHash,
    /// Block time in seconds since the Unix epoch.
    pub time: u32,
    /// Sapling tree.
    pub sapling: Option<Frontier>,
    /// Orchard tree.
    pub orchard: Option<Frontier>,
    /// Ironwood tree.
    pub ironwood: Option<Frontier>,
}

/// The root of a completed subtree of a note commitment tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubtreeRoot {
    /// Root of the subtree.
    pub root: TreeNode,
    /// Hash of the block that completed the subtree.
    pub completing_block_hash: BlockHash,
    /// Height of the block that completed the subtree.
    pub completing_block_height: BlockHeight,
}

/// A full serialized transaction.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RawTransaction {
    /// Transaction bytes.
    pub data: Vec<u8>,
    /// Height of the block that holds the transaction. `None` for a mempool transaction.
    pub mined_height: Option<BlockHeight>,
}

/// Information about a source and its chain.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ServerInfo {
    /// Name of the server software.
    pub vendor: String,
    /// Version of the server software.
    pub version: String,
    /// Name of the chain.
    pub chain_name: String,
    /// Height of the chain tip.
    pub block_height: BlockHeight,
    /// Estimated height of the network.
    pub estimated_height: BlockHeight,
    /// Sapling activation height.
    pub sapling_activation_height: BlockHeight,
    /// Consensus branch ID as hex.
    pub consensus_branch_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_array_rejects_a_wrong_length() {
        let error = Nullifier::try_from(&[0u8; 31][..]).unwrap_err();
        assert_eq!((error.expected(), error.actual()), (32, 31));
        assert_eq!(error.to_string(), "expected 32 bytes, found 31");

        assert!(CompactCiphertext::try_from(&[0u8; 52][..]).is_ok());
        assert!(CompactCiphertext::try_from(&[0u8; 32][..]).is_err());
    }

    #[test]
    fn block_hash_prints_in_reversed_byte_order() {
        let mut bytes = [0u8; 32];
        bytes[0] = 0xab;
        assert!(BlockHash::from(bytes).to_string().ends_with("ab"));
        assert!(Nullifier::from(bytes).to_string().starts_with("ab"));
        assert!(format!("{:?}", BlockHash::from(bytes)).starts_with("BlockHash(00"));
    }

    #[test]
    fn frontier_needs_one_ommer_for_each_one_bit_of_the_position() {
        let node = TreeNode::default();
        assert!(Frontier::new(0, node, vec![]).is_ok());
        assert!(Frontier::new(5, node, vec![node, node]).is_ok());
        assert!(Frontier::new(5, node, vec![node]).is_err());
        assert!(Frontier::new(0, node, vec![node]).is_err());
    }
}
