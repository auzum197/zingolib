//! Module for handling all connections to the server

use std::{ops::Range, time::Duration};

use tokio::sync::{mpsc::UnboundedSender, oneshot};
use tokio_util::sync::CancellationToken;

use zcash_primitives::transaction::{Transaction, TxId};
use zcash_protocol::consensus::{self, BlockHeight};

#[cfg(not(feature = "darkside_test"))]
use futures::TryStreamExt as _;
use wallet_api::{
    Fetched, IntegrityBasis, ResponseStream, SourceError, SourceId, SourceSet,
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

/// Which integrity basis a sync run needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Policy {
    Auto,
    AssumedOnly,
    Verified,
}

/// A block range, and its source if the blocks need verification.
pub struct BlockStream {
    /// The compact blocks.
    pub blocks: ResponseStream<CompactBlock>,
    /// The source of the blocks, if they need verification.
    pub unverified_source: Option<SourceId>,
}

impl Policy {
    pub(crate) fn admit<T>(
        self,
        fetched: Result<Fetched<T>, SourceError>,
    ) -> Result<T, ServerError> {
        let fetched = fetched?;
        if self == Policy::AssumedOnly && fetched.basis != IntegrityBasis::Assumed {
            return Err(ServerError::IntegrityRequired);
        }

        Ok(fetched.value)
    }

    /// `checkable` is whether the tree states have a better basis than one unverified source.
    /// Without that, the check of a block range proves nothing.
    pub(crate) fn admit_blocks(
        self,
        fetched: Result<Fetched<ResponseStream<CompactBlock>>, SourceError>,
        checkable: bool,
    ) -> Result<BlockStream, ServerError> {
        let fetched = fetched?;
        let assumed = fetched.basis == IntegrityBasis::Assumed;
        let unverified_source = match self {
            Policy::AssumedOnly if !assumed => return Err(ServerError::IntegrityRequired),
            Policy::Verified if !checkable => return Err(ServerError::UnverifiedTreeState),
            Policy::Verified => Some(fetched.source),
            Policy::Auto if !assumed && checkable => Some(fetched.source),
            Policy::Auto | Policy::AssumedOnly => None,
        };

        Ok(BlockStream {
            unverified_source,
            blocks: fetched.value,
        })
    }

    /// A tree state for the check of a block range, or `None` if the check must not run.
    pub(crate) fn admit_boundary<T>(
        self,
        fetched: Result<Fetched<T>, SourceError>,
    ) -> Result<Option<T>, ServerError> {
        let fetched = fetched?;
        match (fetched.basis, self) {
            (IntegrityBasis::Unverified, Policy::Verified) => Err(ServerError::UnverifiedTreeState),
            (IntegrityBasis::Unverified, _) => Ok(None),
            _ => Ok(Some(fetched.value)),
        }
    }
}

#[cfg(not(feature = "darkside_test"))]
const MAX_RETRIES: u8 = 3;

const FETCH_REPLY_TIMEOUT: Duration = Duration::from_secs(10);
#[cfg(not(feature = "darkside_test"))]
const STREAM_MSG_TIMEOUT: Duration = Duration::from_secs(15);

async fn recv_fetch_reply<T>(
    rx: oneshot::Receiver<Result<T, ServerError>>,
    what: &'static str,
) -> Result<T, ServerError> {
    match tokio::time::timeout(FETCH_REPLY_TIMEOUT, rx).await {
        Ok(res) => res.map_err(|_| ServerError::FetcherDropped)?,
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
    ChainTip(oneshot::Sender<Result<BlockId, ServerError>>),
    /// Gets  a compact block of the given block height.
    CompactBlock(
        oneshot::Sender<Result<CompactBlock, ServerError>>,
        BlockHeight,
    ),
    /// Gets the specified range of compact blocks from the server (end exclusive).
    CompactBlockRange(
        oneshot::Sender<Result<BlockStream, ServerError>>,
        Range<BlockHeight>,
    ),
    /// Gets the specified range of nullifiers from the server (end exclusive).
    NullifierRange(
        oneshot::Sender<Result<BlockStream, ServerError>>,
        Range<BlockHeight>,
    ),
    /// Gets the tree states for a specified block height.
    TreeState(oneshot::Sender<Result<TreeState, ServerError>>, BlockHeight),
    /// Gets the tree states for the check of a block range. `None` if the check must not run.
    BoundaryTreeState(
        oneshot::Sender<Result<Option<TreeState>, ServerError>>,
        BlockHeight,
    ),
    /// Get a full transaction by txid.
    Transaction(oneshot::Sender<Result<RawTransaction, ServerError>>, TxId),
    /// Tells the router that blocks from this source failed verification.
    FailedVerification(SourceId),
    /// Get a list of transactions for a given transparent address and block range.
    TransparentAddressTxs(
        oneshot::Sender<Result<ResponseStream<RawTransaction>, ServerError>>,
        (String, Range<BlockHeight>),
    ),
    /// Get a stream of shards.
    #[cfg(not(feature = "darkside_test"))]
    SubtreeRoots(
        oneshot::Sender<Result<ResponseStream<SubtreeRoot>, ServerError>>,
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
) -> Result<BlockStream, ServerError> {
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
) -> Result<BlockStream, ServerError> {
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

    Ok(tree_state.into())
}

/// Gets the frontiers at `block_height` for the check of a block range. `None` if the check
/// must not run.
pub(crate) async fn get_boundary_frontiers(
    fetch_request_sender: UnboundedSender<FetchRequest>,
    block_height: BlockHeight,
) -> Result<Option<Frontiers>, ServerError> {
    let (reply_sender, reply_receiver) = oneshot::channel();
    fetch_request_sender
        .send(FetchRequest::BoundaryTreeState(reply_sender, block_height))
        .map_err(|_| ServerError::FetcherDropped)?;

    let tree_state = recv_fetch_reply(reply_receiver, "TreeState").await?;

    Ok(tree_state.map(Into::into))
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
    if transaction.txid() != txid {
        return Err(ServerError::InvalidTransaction(std::io::Error::other(
            "transaction does not match the requested txid",
        )));
    }

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
    policy: Policy,
    shutdown: &CancellationToken,
) -> Result<ResponseStream<RawTransaction>, MempoolError> {
    tracing::debug!("Fetching mempool stream");
    tokio::select! {
        biased;
        mempool_stream_response = sources.mempool() => {
            Ok(policy.admit(mempool_stream_response)?)
        }
        _ = shutdown.cancelled() => Err(MempoolError::ShutdownWithoutStream),
    }
}

#[cfg(test)]
mod tests {
    use wallet_api::SourceId;

    use super::*;

    fn fetched(basis: IntegrityBasis) -> Result<Fetched<u8>, SourceError> {
        Ok(Fetched {
            value: 1,
            source: SourceId(0),
            basis,
        })
    }

    #[test]
    fn assumed_only_policy_rejects_results_from_other_sources() {
        assert!(matches!(
            Policy::AssumedOnly.admit(fetched(IntegrityBasis::Quorum(2))),
            Err(ServerError::IntegrityRequired)
        ));
        assert!(matches!(
            Policy::AssumedOnly.admit(fetched(IntegrityBasis::Assumed)),
            Ok(1)
        ));
        assert!(matches!(
            Policy::Auto.admit(fetched(IntegrityBasis::Unverified)),
            Ok(1)
        ));
        assert!(matches!(
            Policy::AssumedOnly.admit_blocks(blocks(IntegrityBasis::Unverified), true),
            Err(ServerError::IntegrityRequired)
        ));
    }

    fn blocks(basis: IntegrityBasis) -> Result<Fetched<ResponseStream<CompactBlock>>, SourceError> {
        Ok(Fetched {
            value: Box::pin(futures::stream::empty()),
            source: SourceId(3),
            basis,
        })
    }

    fn serve(transaction: Vec<u8>) -> UnboundedSender<FetchRequest> {
        let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(FetchRequest::Transaction(reply, _)) = receiver.recv().await {
                let _ignore_error = reply.send(Ok(RawTransaction {
                    data: transaction.clone(),
                    mined_height: Some(BlockHeight::from_u32(1_687_104)),
                }));
            }
        });
        sender
    }

    #[tokio::test]
    async fn transaction_must_match_the_requested_txid() {
        use zcash_primitives::transaction::{TransactionData, TxVersion};
        use zcash_protocol::consensus::{BranchId, MAIN_NETWORK};

        let transaction = TransactionData::from_parts(
            TxVersion::V5,
            BranchId::Nu5,
            0,
            BlockHeight::from_u32(0),
            None,
            None,
            None,
            None,
        )
        .freeze()
        .unwrap();
        let mut bytes = Vec::new();
        transaction.write(&mut bytes).unwrap();

        let fetched = get_transaction_and_block_height(
            serve(bytes.clone()),
            &MAIN_NETWORK,
            transaction.txid(),
        )
        .await
        .unwrap();
        assert_eq!(fetched.0.txid(), transaction.txid());

        let other = get_transaction_and_block_height(
            serve(bytes),
            &MAIN_NETWORK,
            TxId::from_bytes([7; 32]),
        )
        .await;
        assert!(matches!(other, Err(ServerError::InvalidTransaction(_))));
    }

    #[test]
    fn unverified_blocks_keep_their_source_for_verification() {
        let unverified_source = |policy: Policy, basis| {
            policy
                .admit_blocks(blocks(basis), true)
                .unwrap()
                .unverified_source
        };

        assert_eq!(
            unverified_source(Policy::Auto, IntegrityBasis::Assumed),
            None
        );
        assert_eq!(
            unverified_source(Policy::Auto, IntegrityBasis::Unverified),
            Some(SourceId(3))
        );
        assert_eq!(
            unverified_source(Policy::Verified, IntegrityBasis::Assumed),
            Some(SourceId(3))
        );
    }

    #[test]
    fn blocks_are_not_checked_against_unverified_tree_states() {
        let admit = |policy: Policy, basis| policy.admit_blocks(blocks(basis), false);

        let unchecked = admit(Policy::Auto, IntegrityBasis::Unverified).unwrap();
        assert_eq!(unchecked.unverified_source, None);
        assert!(matches!(
            admit(Policy::Verified, IntegrityBasis::Assumed),
            Err(ServerError::UnverifiedTreeState)
        ));
        assert!(admit(Policy::AssumedOnly, IntegrityBasis::Assumed).is_ok());
    }

    #[test]
    fn boundary_tree_state_needs_a_better_basis_than_one_unverified_source() {
        let admit = |policy: Policy, basis| {
            policy
                .admit_boundary(fetched(basis))
                .map(|tree_state| tree_state.is_some())
        };

        assert!(matches!(
            admit(Policy::Auto, IntegrityBasis::Quorum(2)),
            Ok(true)
        ));
        assert!(matches!(
            admit(Policy::Verified, IntegrityBasis::Assumed),
            Ok(true)
        ));
        assert!(matches!(
            admit(Policy::Auto, IntegrityBasis::Unverified),
            Ok(false)
        ));
        assert!(matches!(
            admit(Policy::Verified, IntegrityBasis::Unverified),
            Err(ServerError::UnverifiedTreeState)
        ));
    }
}
