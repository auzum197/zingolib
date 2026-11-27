//! Requests to a chain source and their responses.

use std::{fmt, ops::Range};

use futures::stream::BoxStream;
use zcash_protocol::{ShieldedPool, TxId, consensus::BlockHeight};

use crate::{
    RequestKind, SourceError,
    chain::{BlockId, CompactBlock, RawTransaction, ServerInfo, SubtreeRoot, TreeState},
};

/// A stream of items from a source.
pub type ResponseStream<T> = BoxStream<'static, Result<T, SourceError>>;

/// Block ranges exclude the end height.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Request {
    /// Server information.
    Info,
    /// Height and hash of the chain tip.
    Tip,
    /// One compact block.
    Block(BlockHeight),
    /// A range of compact blocks.
    Blocks(Range<BlockHeight>),
    /// A range of compact blocks with nullifiers only.
    Nullifiers(Range<BlockHeight>),
    /// Note commitment tree state at the end of a block.
    TreeState(BlockHeight),
    /// Roots of completed subtrees.
    SubtreeRoots {
        /// Shielded pool of the tree.
        pool: ShieldedPool,
        /// Index of the first subtree.
        start_index: u32,
        /// Maximum number of roots. Zero means all.
        max_entries: u32,
    },
    /// A full transaction by txid.
    Transaction(TxId),
    /// Transactions of a transparent address in a block range.
    AddressTransactions {
        /// Encoded transparent address.
        address: String,
        /// Block range.
        range: Range<BlockHeight>,
    },
    /// Mempool transactions until the next block.
    Mempool,
    /// Sends a serialized transaction to the network.
    Broadcast(Vec<u8>),
}

/// Wallet data that a request exposes to the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Disclosure {
    /// Nothing about the wallet.
    None,
    /// A transaction the wallet has interest in.
    Txid,
    /// An address of the wallet.
    Address,
    /// A transaction the wallet sends.
    Spend,
}

impl Request {
    /// The type of this request.
    pub fn kind(&self) -> RequestKind {
        match self {
            Request::Info => RequestKind::Info,
            Request::Tip => RequestKind::Tip,
            Request::Block(_) => RequestKind::Block,
            Request::Blocks(_) => RequestKind::Blocks,
            Request::Nullifiers(_) => RequestKind::Nullifiers,
            Request::TreeState(_) => RequestKind::TreeState,
            Request::SubtreeRoots { .. } => RequestKind::SubtreeRoots,
            Request::Transaction(_) => RequestKind::Transaction,
            Request::AddressTransactions { .. } => RequestKind::AddressTransactions,
            Request::Mempool => RequestKind::Mempool,
            Request::Broadcast(_) => RequestKind::Broadcast,
        }
    }

    /// Wallet data that this request exposes to the source.
    pub fn disclosure(&self) -> Disclosure {
        match self {
            Request::Transaction(_) => Disclosure::Txid,
            Request::AddressTransactions { .. } => Disclosure::Address,
            Request::Broadcast(_) => Disclosure::Spend,
            _ => Disclosure::None,
        }
    }
}

/// The response to a [`Request`].
#[non_exhaustive]
pub enum Response {
    /// Response to [`Request::Info`].
    Info(ServerInfo),
    /// Response to [`Request::Tip`].
    Tip(BlockId),
    /// Response to [`Request::Block`].
    Block(CompactBlock),
    /// Response to [`Request::Blocks`] and [`Request::Nullifiers`].
    Blocks(ResponseStream<CompactBlock>),
    /// Response to [`Request::TreeState`].
    TreeState(TreeState),
    /// Response to [`Request::SubtreeRoots`].
    SubtreeRoots(ResponseStream<SubtreeRoot>),
    /// Response to [`Request::Transaction`].
    Transaction(RawTransaction),
    /// Response to [`Request::AddressTransactions`] and [`Request::Mempool`].
    Transactions(ResponseStream<RawTransaction>),
    /// The ID of the submitted transaction, from the source.
    Broadcast(TxId),
}

impl fmt::Debug for Response {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Response::Info(_) => "Info",
            Response::Tip(_) => "Tip",
            Response::Block(_) => "Block",
            Response::Blocks(_) => "Blocks",
            Response::TreeState(_) => "TreeState",
            Response::SubtreeRoots(_) => "SubtreeRoots",
            Response::Transaction(_) => "Transaction",
            Response::Transactions(_) => "Transactions",
            Response::Broadcast(_) => "Broadcast",
        })
    }
}
