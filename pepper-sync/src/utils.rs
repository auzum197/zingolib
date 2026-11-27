use wallet_api::chain::{CompactBlock, CompactTx};
use zcash_primitives::block::BlockHash;
use zcash_protocol::{TxId, consensus::BlockHeight};

/// Returns the [`BlockHash`] for this block.
pub(crate) fn get_compact_block_hash(compact_block: &CompactBlock) -> BlockHash {
    compact_block.hash
}

/// Returns the [`BlockHash`] for this block's parent.
pub(crate) fn get_compact_block_prev_hash(compact_block: &CompactBlock) -> BlockHash {
    compact_block.prev_hash
}

/// Returns the [`BlockHeight`] value for this block
pub(crate) fn get_compact_block_height(compact_block: &CompactBlock) -> BlockHeight {
    compact_block.height
}

pub(crate) fn get_compact_tx_txid(compact_tx: &CompactTx) -> TxId {
    compact_tx.txid
}
