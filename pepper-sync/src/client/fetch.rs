//! Queue and prioritise fetch requests to fetch data from the server

use tokio::sync::mpsc::UnboundedReceiver;

use tracing::instrument;
use wallet_api::{
    IntegrityBasis, Request, SourceError, SourceSet,
    chain::{BlockId, TreeState},
};
use zcash_protocol::consensus::BlockHeight;

use crate::client::{FetchRequest, Policy};
use crate::error::ServerError;

/// Receives [`self::FetchRequest`]'s via an [`tokio::sync::mpsc::UnboundedReceiver`] for queueing,
/// prioritisation and fetching from the server.
/// Returns the data specified in the [`self::FetchRequest`] variant via the provided [`tokio::sync::oneshot::Sender`].
///
/// Allows all requests to the server to be handled from a single task for efficiency and also enables
/// request prioritisation for further performance enhancement
pub(crate) async fn fetch(
    mut fetch_request_receiver: UnboundedReceiver<FetchRequest>,
    sources: std::sync::Arc<SourceSet>,
    policy: Policy,
) {
    let mut fetch_request_queue: Vec<FetchRequest> = Vec::new();

    loop {
        // `fetch` returns here when all requests have successfully been fetched and the
        // fetch_request channel is closed on sync completion.
        if receive_fetch_requests(&mut fetch_request_receiver, &mut fetch_request_queue).await {
            return;
        }

        let fetch_request = select_fetch_request(&mut fetch_request_queue);

        if let Some(request) = fetch_request {
            fetch_from_server(&sources, policy, request).await;
        }
    }
}

// receives fetch requests and populates the fetch request queue
//
// returns `true` if the fetch request channel is closed and all fetch requests have been completed,
// signalling sync is complete and no longer needs to fetch data from the server.
async fn receive_fetch_requests(
    receiver: &mut UnboundedReceiver<FetchRequest>,
    fetch_request_queue: &mut Vec<FetchRequest>,
) -> bool {
    // if there are no fetch requests to process, sleep until the next fetch request is received
    // or channel is closed
    if fetch_request_queue.is_empty()
        && let Some(fetch_request) = receiver.recv().await
    {
        fetch_request_queue.push(fetch_request);
    }
    // receive all remaining fetch requests from channel
    // when channel is empty return `false` to continue fetching data from the server
    // when channel is closed and all fetch requests are processed, return `true`
    loop {
        match receiver.try_recv() {
            Ok(fetch_request) => fetch_request_queue.push(fetch_request),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {
                if fetch_request_queue.is_empty() {
                    return true;
                }
                break;
            }
        }
    }

    false
}

// TODO: placeholder for algorithm that selects the next fetch request to be processed
// return `None` if a fetch request could not be selected
fn select_fetch_request(fetch_request_queue: &mut Vec<FetchRequest>) -> Option<FetchRequest> {
    // TODO: improve priority logic
    if fetch_request_queue.is_empty() {
        None
    } else {
        Some(fetch_request_queue.remove(0))
    }
}

async fn fetch_from_server(sources: &SourceSet, policy: Policy, fetch_request: FetchRequest) {
    match fetch_request {
        FetchRequest::ChainTip(sender) => {
            tracing::debug!("Fetching chain tip.");
            let _ignore_error = sender.send(get_latest_block(sources, policy).await);
        }
        FetchRequest::CompactBlock(sender, block_height) => {
            tracing::debug!("Fetching compact block. {:?}", &block_height);
            let block = policy.admit(sources.block(block_height).await);
            let _ignore_error = sender.send(block);
        }
        FetchRequest::CompactBlockRange(sender, block_range) => {
            tracing::debug!("Fetching compact blocks. {:?}", &block_range);
            let checkable = checkable(sources, block_range.end - 1);
            let block_stream = policy.admit_blocks(sources.blocks(block_range).await, checkable);
            let _ignore_error = sender.send(block_stream);
        }
        FetchRequest::NullifierRange(sender, block_range) => {
            tracing::debug!("Fetching nullifiers. {:?}", &block_range);
            let checkable = checkable(sources, block_range.end - 1);
            let block_stream =
                policy.admit_blocks(sources.nullifiers(block_range).await, checkable);
            let _ignore_error = sender.send(block_stream);
        }
        #[cfg(not(feature = "darkside_test"))]
        FetchRequest::SubtreeRoots(sender, start_index, pool, max_entries) => {
            tracing::debug!(
                "Fetching subtree roots. start index: {}. shielded pool: {:?}",
                start_index,
                pool
            );
            let subtree_roots =
                policy.admit(sources.subtree_roots(pool, start_index, max_entries).await);
            let _ignore_error = sender.send(subtree_roots);
        }
        FetchRequest::TreeState(sender, block_height) => {
            tracing::debug!("Fetching tree state. {:?}", &block_height);
            let _ignore_error = sender.send(get_tree_state(sources, policy, block_height).await);
        }
        FetchRequest::BoundaryTreeState(sender, block_height) => {
            tracing::debug!("Fetching boundary tree state. {:?}", &block_height);
            let tree_state = policy.admit_boundary(sources.tree_state(block_height).await);
            let _ignore_error = sender.send(tree_state);
        }
        FetchRequest::Transaction(sender, txid) => {
            tracing::debug!("Fetching transaction. {:?}", txid);
            let transaction = policy.admit(sources.transaction(txid).await);
            let _ignore_error = sender.send(transaction);
        }
        FetchRequest::FailedVerification(source) => {
            tracing::warn!("Blocks from source {} failed verification.", source.0);
            sources.on_failure(source, &SourceError::FailedVerification);
        }
        FetchRequest::TransparentAddressTxs(sender, (address, block_range)) => {
            tracing::debug!(
                "Fetching raw transactions in block range {:?} for address {:?}",
                &block_range,
                &address
            );
            let raw_transaction_stream =
                policy.admit(sources.address_transactions(address, block_range).await);
            let _ignore_error = sender.send(raw_transaction_stream);
        }
    }
}

/// Whether the tree state at `block_height` will have a better basis than one unverified source.
fn checkable(sources: &SourceSet, block_height: BlockHeight) -> bool {
    sources
        .route_info(&Request::TreeState(block_height))
        .is_ok_and(|route| route.basis != IntegrityBasis::Unverified)
}

#[instrument(
    skip(sources, policy),
    name = "fetch::get_latest_block",
    err,
    level = "info"
)]
async fn get_latest_block(sources: &SourceSet, policy: Policy) -> Result<BlockId, ServerError> {
    policy.admit(sources.tip().await)
}

#[instrument(
    skip(sources, policy),
    name = "fetch::get_tree_state",
    err,
    level = "info"
)]
async fn get_tree_state(
    sources: &SourceSet,
    policy: Policy,
    block_height: BlockHeight,
) -> Result<TreeState, ServerError> {
    policy.admit(sources.tree_state(block_height).await)
}
