//! Chain data that a source returns.
//!
//! The values are types of the Zcash protocol crates, so a value that exists is valid. A
//! source converts its own format into them and returns [`crate::SourceError::InvalidData`]
//! for data that does not convert. The structs in this module only group those values.

use std::fmt;

use incrementalmerkletree::frontier::Frontier;
use orchard::tree::MerkleHashOrchard;
use zcash_note_encryption::{COMPACT_NOTE_SIZE, ShieldedOutput};
use zcash_protocol::{TxId, consensus::BlockHeight};

pub use orchard::note_encryption::CompactAction;
pub use sapling_crypto::note_encryption::CompactOutputDescription;
pub use zcash_primitives::block::BlockHash;
pub use zcash_transparent::bundle::{OutPoint, TxOut};

/// The frontier of the Sapling note commitment tree.
pub type SaplingFrontier =
    Frontier<sapling_crypto::Node, { sapling_crypto::NOTE_COMMITMENT_TREE_DEPTH }>;

/// The frontier of the Orchard or the Ironwood note commitment tree.
pub type OrchardFrontier =
    Frontier<MerkleHashOrchard, { orchard::NOTE_COMMITMENT_TREE_DEPTH as u8 }>;

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
#[derive(Clone)]
pub struct CompactTx {
    /// Position of the transaction in its block.
    pub index: u64,
    /// Transaction ID.
    pub txid: TxId,
    /// Transaction fee in zatoshis.
    pub fee: u32,
    /// Nullifiers of the Sapling spends.
    pub spends: Vec<sapling_crypto::Nullifier>,
    /// Sapling outputs.
    pub outputs: Vec<CompactOutputDescription>,
    /// Orchard actions.
    pub actions: Vec<CompactAction>,
    /// Ironwood actions.
    pub ironwood_actions: Vec<CompactAction>,
    /// Outpoints that the transparent inputs spend.
    pub transparent_inputs: Vec<OutPoint>,
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

/// The commitment, the ephemeral key and the ciphertext of a compact output.
type OutputBytes = ([u8; 32], [u8; 32], [u8; COMPACT_NOTE_SIZE]);

/// The bytes of a compact output or action. The protocol types have no equality.
fn output_bytes<D, O>(output: &O) -> OutputBytes
where
    D: zcash_note_encryption::Domain<ExtractedCommitmentBytes = [u8; 32]>,
    O: ShieldedOutput<D, COMPACT_NOTE_SIZE>,
{
    (
        output.cmstar_bytes(),
        output.ephemeral_key().0,
        *output.enc_ciphertext(),
    )
}

fn action_bytes(action: &CompactAction) -> ([u8; 32], OutputBytes) {
    (
        action.nullifier().to_bytes(),
        output_bytes::<orchard::note_encryption::OrchardDomain, _>(action),
    )
}

impl PartialEq for CompactTx {
    fn eq(&self, other: &Self) -> bool {
        let outputs = |tx: &Self| -> Vec<_> {
            tx.outputs
                .iter()
                .map(output_bytes::<sapling_crypto::note_encryption::SaplingDomain, _>)
                .collect()
        };
        let actions =
            |actions: &[CompactAction]| -> Vec<_> { actions.iter().map(action_bytes).collect() };

        self.index == other.index
            && self.txid == other.txid
            && self.fee == other.fee
            && self.spends == other.spends
            && outputs(self) == outputs(other)
            && actions(&self.actions) == actions(&other.actions)
            && actions(&self.ironwood_actions) == actions(&other.ironwood_actions)
            && self.transparent_inputs == other.transparent_inputs
            && self.transparent_outputs == other.transparent_outputs
    }
}

impl Eq for CompactTx {}

impl fmt::Debug for CompactTx {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CompactTx")
            .field("index", &self.index)
            .field("txid", &self.txid)
            .field("fee", &self.fee)
            .field("spends", &self.spends.len())
            .field("outputs", &self.outputs.len())
            .field("actions", &self.actions.len())
            .field("ironwood_actions", &self.ironwood_actions.len())
            .field("transparent_inputs", &self.transparent_inputs.len())
            .field("transparent_outputs", &self.transparent_outputs.len())
            .finish()
    }
}

/// The note commitment trees at the end of a block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeState {
    /// Block height.
    pub height: BlockHeight,
    /// Block hash.
    pub hash: BlockHash,
    /// Block time in seconds since the Unix epoch.
    pub time: u32,
    /// Sapling tree.
    pub sapling: SaplingFrontier,
    /// Orchard tree.
    pub orchard: OrchardFrontier,
    /// Ironwood tree.
    pub ironwood: OrchardFrontier,
}

/// The root of a completed subtree of a note commitment tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SubtreeRoot {
    /// Root of the subtree. Its type depends on the pool of the request, so it is bytes.
    pub root: [u8; 32],
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
    use zcash_note_encryption::EphemeralKeyBytes;

    use super::*;

    fn tx(ciphertext: u8) -> CompactTx {
        let nullifier = orchard::note::Nullifier::from_bytes(&[0; 32]).unwrap();
        let cmx = orchard::note::ExtractedNoteCommitment::from_bytes(&[0; 32]).unwrap();
        CompactTx {
            outputs: vec![CompactOutputDescription {
                cmu: sapling_crypto::note::ExtractedNoteCommitment::from_bytes(&[0; 32]).unwrap(),
                ephemeral_key: EphemeralKeyBytes([0; 32]),
                enc_ciphertext: [ciphertext; COMPACT_NOTE_SIZE],
            }],
            actions: vec![CompactAction::from_parts(
                nullifier,
                cmx,
                EphemeralKeyBytes([0; 32]),
                [ciphertext; COMPACT_NOTE_SIZE],
            )],
            ..CompactTx::new(0, TxId::from_bytes([0; 32]))
        }
    }

    #[test]
    fn transactions_are_equal_only_with_equal_output_and_action_bytes() {
        assert_eq!(tx(1), tx(1));
        assert_ne!(tx(1), tx(2));

        let mut no_action = tx(1);
        no_action.actions.clear();
        assert_ne!(tx(1), no_action);
    }
}
