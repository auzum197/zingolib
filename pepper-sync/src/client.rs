//! Module for handling all connections to the server

use std::{ops::Range, time::Duration};

use tokio::sync::{mpsc::UnboundedSender, oneshot};
use tokio_util::sync::CancellationToken;

use zcash_primitives::transaction::{Transaction, TxId};
use zcash_protocol::consensus::{self, BlockHeight};

use futures::TryStreamExt as _;
use wallet_api::{
    ResponseStream, SourceError, SourceSet,
    chain::{BlockId, CompactBlock, RawTransaction, TreeState},
};

use crate::{
    error::{MempoolError, ServerError},
    witness::Frontiers,
};

#[cfg(not(feature = "darkside_test"))]
use wallet_api::chain::SubtreeRoot;
#[cfg(not(feature = "darkside_test"))]
use zcash_protocol::ShieldedPool;

pub(crate) mod fetch;

#[cfg(not(feature = "darkside_test"))]
const MAX_RETRIES: u8 = 3;

const FETCH_REPLY_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(not(feature = "darkside_test"))]
const STREAM_MSG_TIMEOUT: Duration = Duration::from_secs(15);

async fn recv_fetch_reply<T>(
    rx: oneshot::Receiver<Result<T, SourceError>>,
    what: &'static str,
) -> Result<T, ServerError> {
    match tokio::time::timeout(FETCH_REPLY_TIMEOUT, rx).await {
        Ok(res) => {
            let inner = res.map_err(|_| ServerError::FetcherDropped)?;
            inner.map_err(Into::into)
        }
        Err(_) => Err(SourceError::Timeout(format!("fetch {what} reply timeout")).into()),
    }
}

#[cfg(not(feature = "darkside_test"))]
async fn next_stream_item<T>(
    stream: &mut ResponseStream<T>,
    what: &'static str,
) -> Result<Option<T>, SourceError> {
    match tokio::time::timeout(STREAM_MSG_TIMEOUT, stream.try_next()).await {
        Ok(res) => res,
        Err(_) => Err(SourceError::Timeout(format!(
            "{what} stream message timeout"
        ))),
    }
}

/// Fetch requests are created and sent to the [`crate::client::fetch::fetch`] task when a connection to the server is required.
///
/// Each variant includes a [`tokio::sync::oneshot::Sender`] for returning the fetched data to the requester.
pub enum FetchRequest {
    /// Gets the height of the blockchain from the server.
    ChainTip(oneshot::Sender<Result<BlockId, SourceError>>),
    /// Gets  a compact block of the given block height.
    CompactBlock(
        oneshot::Sender<Result<CompactBlock, SourceError>>,
        BlockHeight,
    ),
    /// Gets the specified range of compact blocks from the server (end exclusive).
    CompactBlockRange(
        oneshot::Sender<Result<ResponseStream<CompactBlock>, SourceError>>,
        Range<BlockHeight>,
    ),
    /// Gets the specified range of nullifiers from the server (end exclusive).
    NullifierRange(
        oneshot::Sender<Result<ResponseStream<CompactBlock>, SourceError>>,
        Range<BlockHeight>,
    ),
    /// Gets the tree states for a specified block height.
    TreeState(oneshot::Sender<Result<TreeState, SourceError>>, BlockHeight),
    /// Get a full transaction by txid.
    Transaction(oneshot::Sender<Result<RawTransaction, SourceError>>, TxId),
    /// Get a list of transactions for a given transparent address and block range.
    TransparentAddressTxs(
        oneshot::Sender<Result<ResponseStream<RawTransaction>, SourceError>>,
        (String, Range<BlockHeight>),
    ),
    /// Get a stream of shards.
    #[cfg(not(feature = "darkside_test"))]
    SubtreeRoots(
        oneshot::Sender<Result<ResponseStream<SubtreeRoot>, SourceError>>,
        u32,
        ShieldedPool,
        u32,
    ),
}

/// Gets the height of the blockchain from the server.
///
/// Requires [`crate::client::fetch::fetch`] to be running concurrently, connected via the `fetch_request` channel.
pub(crate) async fn get_chain_height(
    fetch_request_sender: UnboundedSender<FetchRequest>,
) -> Result<BlockHeight, ServerError> {
    let (reply_sender, reply_receiver) = oneshot::channel();
    fetch_request_sender
        .send(FetchRequest::ChainTip(reply_sender))
        .map_err(|_| ServerError::FetcherDropped)?;
    let chain_tip = match tokio::time::timeout(FETCH_REPLY_TIMEOUT, reply_receiver).await {
        Ok(res) => res.map_err(|_| ServerError::FetcherDropped)??,
        Err(_) => {
            return Err(SourceError::Timeout("fetch ChainTip reply timeout".into()).into());
        }
    };

    Ok(chain_tip.height)
}

/// Gets the compact block of the given block height from the server.
///
/// Requires [`crate::client::fetch::fetch`] to be running concurrently, connected via the `fetch_request` channel.
pub(crate) async fn get_compact_block(
    fetch_request_sender: UnboundedSender<FetchRequest>,
    block_height: BlockHeight,
) -> Result<CompactBlock, ServerError> {
    let (reply_sender, reply_receiver) = oneshot::channel();
    fetch_request_sender
        .send(FetchRequest::CompactBlock(reply_sender, block_height))
        .map_err(|_| ServerError::FetcherDropped)?;

    let block = match tokio::time::timeout(FETCH_REPLY_TIMEOUT, reply_receiver).await {
        Ok(res) => res.map_err(|_| ServerError::FetcherDropped)??,
        Err(_) => {
            return Err(SourceError::Timeout("fetch CompactBlock reply timeout".into()).into());
        }
    };

    Ok(block)
}

/// Gets the specified range of compact blocks from the server (end exclusive).
///
/// Requires [`crate::client::fetch::fetch`] to be running concurrently, connected via the `fetch_request` channel.
pub(crate) async fn get_compact_block_range(
    fetch_request_sender: UnboundedSender<FetchRequest>,
    block_range: Range<BlockHeight>,
) -> Result<ResponseStream<CompactBlock>, ServerError> {
    let (reply_sender, reply_receiver) = oneshot::channel();
    fetch_request_sender
        .send(FetchRequest::CompactBlockRange(reply_sender, block_range))
        .map_err(|_| ServerError::FetcherDropped)?;

    let block_stream = match tokio::time::timeout(FETCH_REPLY_TIMEOUT, reply_receiver).await {
        Ok(res) => res.map_err(|_| ServerError::FetcherDropped)??,
        Err(_) => {
            return Err(
                SourceError::Timeout("fetch CompactBlockRange reply timeout".into()).into(),
            );
        }
    };

    Ok(block_stream)
}

/// Gets the specified range of nullifiers from the server (end exclusive).
///
/// Nullifiers are stored in compact blocks where the actions contain only nullifiers.
///
/// Requires [`crate::client::fetch::fetch`] to be running concurrently, connected via the `fetch_request` channel.
pub(crate) async fn get_nullifier_range(
    fetch_request_sender: UnboundedSender<FetchRequest>,
    block_range: Range<BlockHeight>,
) -> Result<ResponseStream<CompactBlock>, ServerError> {
    let (reply_sender, reply_receiver) = oneshot::channel();
    fetch_request_sender
        .send(FetchRequest::NullifierRange(reply_sender, block_range))
        .map_err(|_| ServerError::FetcherDropped)?;

    let block_stream = match tokio::time::timeout(FETCH_REPLY_TIMEOUT, reply_receiver).await {
        Ok(res) => res.map_err(|_| ServerError::FetcherDropped)??,
        Err(_) => {
            return Err(SourceError::Timeout("fetch NullifierRange reply timeout".into()).into());
        }
    };

    Ok(block_stream)
}

/// Gets the stream of shards (subtree roots)
/// from the server.
///
/// Requires [`crate::client::fetch::fetch`] to be running concurrently, connected via the `fetch_request` channel.
#[cfg(not(feature = "darkside_test"))]
pub(crate) async fn get_subtree_roots(
    fetch_request_sender: UnboundedSender<FetchRequest>,
    mut start_index: u32,
    pool: ShieldedPool,
    max_entries: u32,
) -> Result<Vec<SubtreeRoot>, ServerError> {
    let mut subtree_roots = Vec::new();
    let mut retry_count = 0;

    'retry: loop {
        let (reply_sender, reply_receiver) = oneshot::channel();

        fetch_request_sender
            .send(FetchRequest::SubtreeRoots(
                reply_sender,
                start_index,
                pool,
                max_entries,
            ))
            .map_err(|_| ServerError::FetcherDropped)?;

        let mut subtree_root_stream = recv_fetch_reply(reply_receiver, "SubtreeRoots").await?;

        while let Some(subtree_root) =
            match next_stream_item(&mut subtree_root_stream, "SubtreeRoots").await {
                Ok(s) => s,
                Err(SourceError::Timeout(_) | SourceError::Interrupted(_))
                    if retry_count < MAX_RETRIES =>
                {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    retry_count += 1;
                    continue 'retry;
                }
                Err(e) => return Err(e.into()),
            }
        {
            subtree_roots.push(subtree_root);
            start_index += 1;
        }

        break 'retry;
    }

    Ok(subtree_roots)
}

/// Gets the frontiers for a specified block height.
///
/// Requires [`crate::client::fetch::fetch`] to be running concurrently, connected via the `fetch_request` channel.
pub(crate) async fn get_frontiers(
    fetch_request_sender: UnboundedSender<FetchRequest>,
    block_height: BlockHeight,
) -> Result<Frontiers, ServerError> {
    let (reply_sender, reply_receiver) = oneshot::channel();
    fetch_request_sender
        .send(FetchRequest::TreeState(reply_sender, block_height))
        .map_err(|_| ServerError::FetcherDropped)?;

    let tree_state = recv_fetch_reply(reply_receiver, "TreeState").await?;

    tree_state.try_into().map_err(ServerError::InvalidFrontier)
}

/// Gets a full transaction for a specified txid.
///
/// Requires [`crate::client::fetch::fetch`] to be running concurrently, connected via the `fetch_request` channel.
pub(crate) async fn get_transaction_and_block_height(
    fetch_request_sender: UnboundedSender<FetchRequest>,
    consensus_parameters: &impl consensus::Parameters,
    txid: TxId,
) -> Result<(Transaction, BlockHeight), ServerError> {
    let (reply_sender, reply_receiver) = oneshot::channel();
    fetch_request_sender
        .send(FetchRequest::Transaction(reply_sender, txid))
        .map_err(|_| ServerError::FetcherDropped)?;

    let raw_transaction = recv_fetch_reply(reply_receiver, "Transaction").await?;

    let block_height = raw_transaction
        .mined_height
        .unwrap_or(BlockHeight::from_u32(0));

    let transaction = Transaction::read(
        &raw_transaction.data[..],
        consensus::BranchId::for_height(consensus_parameters, block_height),
    )
    .map_err(ServerError::InvalidTransaction)?;

    Ok((transaction, block_height))
}

/// Gets transactions relevant to a given `transparent address` in the specified `block_range`.
///
/// Requires [`crate::client::fetch::fetch`] to be running concurrently, connected via the `fetch_request` channel.
#[cfg(not(feature = "darkside_test"))]
pub(crate) async fn get_transparent_address_transactions(
    fetch_request_sender: UnboundedSender<FetchRequest>,
    consensus_parameters: &impl consensus::Parameters,
    transparent_address: String,
    block_range: Range<BlockHeight>,
) -> Result<Vec<(BlockHeight, Transaction)>, ServerError> {
    let mut raw_transactions: Vec<RawTransaction> = Vec::new();
    let mut retry_count = 0;

    'retry: loop {
        let (reply_sender, reply_receiver) = oneshot::channel();

        fetch_request_sender
            .send(FetchRequest::TransparentAddressTxs(
                reply_sender,
                (transparent_address.clone(), block_range.clone()),
            ))
            .map_err(|_| ServerError::FetcherDropped)?;

        let mut raw_transaction_stream =
            recv_fetch_reply(reply_receiver, "TransparentAddressTxs").await?;

        while let Some(raw_tx) =
            match next_stream_item(&mut raw_transaction_stream, "TransparentAddressTxs").await {
                Ok(s) => s,
                Err(SourceError::Timeout(_) | SourceError::Interrupted(_))
                    if retry_count < MAX_RETRIES =>
                {
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    retry_count += 1;
                    raw_transactions.clear();
                    continue 'retry;
                }
                Err(e) => return Err(e.into()),
            }
        {
            raw_transactions.push(raw_tx);
        }

        break 'retry;
    }

    let transactions = raw_transactions
        .into_iter()
        .map(|raw_transaction| {
            let block_height = raw_transaction
                .mined_height
                .unwrap_or(BlockHeight::from_u32(0));

            let transaction = Transaction::read(
                &raw_transaction.data[..],
                consensus::BranchId::for_height(consensus_parameters, block_height),
            )
            .map_err(ServerError::InvalidTransaction)?;

            Ok((block_height, transaction))
        })
        .collect::<Result<Vec<(BlockHeight, Transaction)>, ServerError>>()?;

    Ok(transactions)
}

/// Gets stream of mempool transactions until the next block is mined.
///
/// Returns [`MempoolError::ShutdownWithoutStream`] if `shutdown` is cancelled before the stream opens.
pub(crate) async fn get_mempool_transaction_stream(
    sources: &SourceSet,
    shutdown: &CancellationToken,
) -> Result<ResponseStream<RawTransaction>, MempoolError> {
    tracing::debug!("Fetching mempool stream");
    tokio::select! {
        biased;
        mempool_stream_response = sources.mempool() => {
            mempool_stream_response
                .map(|stream| stream.value)
                .map_err(|e| MempoolError::ServerError(ServerError::RequestFailed(e)))
        }
        _ = shutdown.cancelled() => Err(MempoolError::ShutdownWithoutStream),
    }
}
