//! Selection of the transactions that hide a wallet transaction in a request.
//!
//! A bucket is a run of blocks. Its limits depend only on the note commitment tree sizes, so
//! each wallet selects the same bucket for a block.

use std::collections::BTreeMap;

use tokio::sync::mpsc;
use zcash_primitives::transaction::TxId;
use zcash_protocol::consensus::{self, BlockHeight};

use crate::{
    client::{self, FetchRequest},
    error::ServerError,
    wallet::{WalletBlock, wallet_block_from_compact_block},
};

const MAX_LEVEL: u32 = 32;

/// Blocks from a scan, and blocks from a source for the heights that the scan does not have.
pub(crate) struct Blocks<'a, P> {
    scanned: &'a BTreeMap<BlockHeight, WalletBlock>,
    fetched: BTreeMap<BlockHeight, WalletBlock>,
    fetch_request_sender: mpsc::UnboundedSender<FetchRequest>,
    consensus_parameters: &'a P,
}

impl<'a, P: consensus::Parameters> Blocks<'a, P> {
    pub(crate) fn new(
        scanned: &'a BTreeMap<BlockHeight, WalletBlock>,
        fetch_request_sender: mpsc::UnboundedSender<FetchRequest>,
        consensus_parameters: &'a P,
    ) -> Self {
        Self {
            scanned,
            fetched: BTreeMap::new(),
            fetch_request_sender,
            consensus_parameters,
        }
    }

    fn get(&self, height: BlockHeight) -> Option<&WalletBlock> {
        self.scanned
            .get(&height)
            .or_else(|| self.fetched.get(&height))
    }

    /// The number of note commitments in all trees at the end of block `height`.
    async fn outputs(&mut self, height: BlockHeight) -> Result<u64, ServerError> {
        if self.get(height).is_none() {
            let block =
                client::get_compact_block(self.fetch_request_sender.clone(), height).await?;
            let block = wallet_block_from_compact_block(
                self.consensus_parameters,
                self.fetch_request_sender.clone(),
                &block,
            )
            .await?;
            self.fetched.insert(height, block);
        }

        Ok(self.get(height).map_or(0, |block| {
            let bounds = block.tree_bounds();
            u64::from(bounds.sapling_final_tree_size)
                + u64::from(bounds.orchard_final_tree_size)
                + u64::from(bounds.ironwood_final_tree_size)
        }))
    }

    /// The txids of the bucket that contains block `height`, in chain sequence.
    ///
    /// A bucket has `minimum` note commitments. Its size doubles until it has `minimum`
    /// transactions or contains all blocks up to `chain_height`.
    pub(crate) async fn bucket_txids(
        &mut self,
        height: BlockHeight,
        chain_height: BlockHeight,
        minimum: usize,
    ) -> Result<Vec<TxId>, ServerError> {
        let first_height = self
            .consensus_parameters
            .activation_height(consensus::NetworkUpgrade::Sapling)
            .unwrap_or(height)
            .min(height);
        let outputs = self.outputs(height).await?;
        let (mut start, mut end) = (height, height);

        for level in 0..=MAX_LEVEL {
            let size = (minimum.max(1) as u64) << level;
            let bucket = outputs / size;
            while start > first_height && self.outputs(start - 1).await? / size == bucket {
                start = start - 1;
            }
            while end < chain_height && self.outputs(end + 1).await? / size == bucket {
                end = end + 1;
            }

            let transactions: usize = self.txids(start, end).count();
            if transactions >= minimum || (start == first_height && end == chain_height) {
                break;
            }
        }

        Ok(self.txids(start, end).copied().collect())
    }

    fn txids(&self, start: BlockHeight, end: BlockHeight) -> impl Iterator<Item = &TxId> {
        (u32::from(start)..=u32::from(end))
            .filter_map(|height| self.get(BlockHeight::from_u32(height)))
            .flat_map(WalletBlock::txids)
    }
}

#[cfg(test)]
mod tests {
    use zcash_primitives::block::BlockHash;
    use zcash_protocol::consensus::{MAIN_NETWORK, NetworkUpgrade, Parameters as _};

    use super::*;
    use crate::{engine::WalletBlockOps as _, wallet::TreeBounds};

    /// One block for each item of `outputs`, which is the tree size at the end of the block.
    /// Each block has one transaction.
    fn chain(first: BlockHeight, outputs: &[u32]) -> BTreeMap<BlockHeight, WalletBlock> {
        outputs
            .iter()
            .enumerate()
            .map(|(index, outputs)| {
                let height = first + index as u32;
                let bounds = TreeBounds {
                    sapling_initial_tree_size: 0,
                    sapling_final_tree_size: 0,
                    orchard_initial_tree_size: 0,
                    orchard_final_tree_size: *outputs,
                    ironwood_initial_tree_size: 0,
                    ironwood_final_tree_size: 0,
                };
                let block = WalletBlock::from_parts(
                    height,
                    BlockHash([0; 32]),
                    BlockHash([0; 32]),
                    0,
                    vec![TxId::from_bytes([index as u8; 32])],
                    bounds,
                );
                (height, block)
            })
            .collect()
    }

    #[tokio::test]
    async fn bucket_depends_on_the_tree_sizes_and_grows_to_the_minimum() {
        let first = MAIN_NETWORK
            .activation_height(NetworkUpgrade::Sapling)
            .unwrap();
        let scanned = chain(first, &[1, 2, 4, 4, 8, 9]);
        let (sender, _receiver) = mpsc::unbounded_channel();
        let mut blocks = Blocks::new(&scanned, sender, &MAIN_NETWORK);
        let txids = |indexes: &[u8]| -> Vec<_> {
            indexes
                .iter()
                .map(|index| TxId::from_bytes([*index; 32]))
                .collect()
        };
        let tip = first + 5;

        let bucket = blocks.bucket_txids(first + 2, tip, 2).await.unwrap();
        assert_eq!(bucket, txids(&[2, 3]));
        let same = blocks.bucket_txids(first + 3, tip, 2).await.unwrap();
        assert_eq!(same, txids(&[2, 3]));

        let grown = blocks.bucket_txids(first, tip, 2).await.unwrap();
        assert_eq!(grown, txids(&[0, 1]));
        let same = blocks.bucket_txids(first + 1, tip, 2).await.unwrap();
        assert_eq!(same, txids(&[0, 1]));

        let all = blocks.bucket_txids(first + 4, tip, 100).await.unwrap();
        assert_eq!(all, txids(&[0, 1, 2, 3, 4, 5]));
    }
}
