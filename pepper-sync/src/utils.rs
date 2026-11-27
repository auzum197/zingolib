use orchard::note_encryption::CompactAction;
use sapling_crypto::{note::ExtractedNoteCommitment, note_encryption::CompactOutputDescription};
use wallet_api::chain::{CompactBlock, CompactOrchardAction, CompactSaplingOutput, CompactTx};
use zcash_note_encryption::EphemeralKeyBytes;
use zcash_primitives::block::BlockHash;
use zcash_protocol::{TxId, consensus::BlockHeight};

use crate::error::CompactFormatError;

/// Returns the [`BlockHash`] for this block.
pub(crate) fn get_compact_block_hash(compact_block: &CompactBlock) -> BlockHash {
    BlockHash(*compact_block.hash.as_bytes())
}

/// Returns the [`BlockHash`] for this block's parent.
pub(crate) fn get_compact_block_prev_hash(compact_block: &CompactBlock) -> BlockHash {
    BlockHash(*compact_block.prev_hash.as_bytes())
}

/// Returns the [`BlockHeight`] value for this block
pub(crate) fn get_compact_block_height(compact_block: &CompactBlock) -> BlockHeight {
    compact_block.height
}

pub(crate) fn get_compact_tx_txid(compact_tx: &CompactTx) -> TxId {
    compact_tx.txid
}

pub(crate) fn get_compact_output_description(
    compact_sapling_output: &CompactSaplingOutput,
) -> Result<CompactOutputDescription, CompactFormatError> {
    let cmu = Option::from(ExtractedNoteCommitment::from_bytes(
        compact_sapling_output.cmu.as_bytes(),
    ))
    .ok_or(CompactFormatError::InvalidValue)?;

    Ok(CompactOutputDescription {
        cmu,
        ephemeral_key: EphemeralKeyBytes(*compact_sapling_output.ephemeral_key.as_bytes()),
        enc_ciphertext: *compact_sapling_output.ciphertext.as_bytes(),
    })
}

pub(crate) fn get_compact_action(
    compact_orchard_action: &CompactOrchardAction,
) -> Result<CompactAction, CompactFormatError> {
    let nullifier = Option::from(orchard::note::Nullifier::from_bytes(
        compact_orchard_action.nullifier.as_bytes(),
    ))
    .ok_or(CompactFormatError::InvalidValue)?;
    let cmx = Option::from(orchard::note::ExtractedNoteCommitment::from_bytes(
        compact_orchard_action.cmx.as_bytes(),
    ))
    .ok_or(CompactFormatError::InvalidValue)?;

    Ok(CompactAction::from_parts(
        nullifier,
        cmx,
        EphemeralKeyBytes(*compact_orchard_action.ephemeral_key.as_bytes()),
        *compact_orchard_action.ciphertext.as_bytes(),
    ))
}
