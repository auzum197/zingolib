//! [`ChainSource`] for [`GrpcIndexer`], and custom connection-based transports for it.

use std::{
    io,
    sync::Arc,
    task::{Context, Poll},
    time::Duration,
};

use chain_router::{
    ChainSource, Connection, Connector, Integrity, Request, Response, SourceError, SourceInfo,
    TransportInfo, WalletIdentifier,
    chain::{self, BlockHash, CompactAction, CompactOutputDescription, OutPoint, TxOut},
};
use futures::{StreamExt, TryStreamExt, future::BoxFuture};
use hyper_util::rt::TokioIo;
use incrementalmerkletree::{Hashable, frontier::Frontier};
use lightwallet_protocol::{
    self as proto, BlockId, BlockRange, ChainSpec, Empty, GetSubtreeRootsArg, RawTransaction,
    TxFilter,
};
use subtle::CtOption;
use tonic::codegen::Service;
use zcash_encoding::CompactSize;
use zcash_note_encryption::EphemeralKeyBytes;
use zcash_primitives::merkle_tree::{HashSer, read_commitment_tree};
use zcash_protocol::{ShieldedPool, TxId, consensus::BlockHeight, value::Zatoshis};
use zcash_transparent::address::Script;

use crate::{GetClientError, GrpcIndexer, Indexer};

const UNARY_TIMEOUT: Duration = Duration::from_secs(10);
const HEAVY_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Clone)]
struct Dial(Arc<dyn Connector>);

impl Service<http::Uri> for Dial {
    type Response = TokioIo<Box<dyn Connection>>;
    type Error = io::Error;
    type Future = BoxFuture<'static, io::Result<Self::Response>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, uri: http::Uri) -> Self::Future {
        let connector = self.0.clone();
        Box::pin(async move {
            let host = uri
                .host()
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "uri has no host"))?;
            let default_port = if uri.scheme_str() == Some("https") {
                443
            } else {
                80
            };
            let port = uri.port_u16().unwrap_or(default_port);

            connector.connect(host, port).await.map(TokioIo::new)
        })
    }
}

impl GrpcIndexer {
    /// Connects through `connector`. TLS still applies to an `https` URI.
    pub fn with_connector(
        uri: http::Uri,
        connector: impl Connector,
    ) -> Result<Self, GetClientError> {
        let endpoint = Self::endpoint(&uri)?;
        let transport = connector.info();
        let channel = endpoint.connect_with_connector_lazy(Dial(Arc::new(connector)));

        Ok(Self::from_channel(uri, channel, transport))
    }

    /// Sets the integrity.
    pub fn with_integrity(mut self, integrity: Integrity) -> Self {
        self.info.integrity = integrity;
        self
    }

    /// Sets the wallet identifiers that the indexer can receive. The default is none.
    pub fn allow_identifiers(mut self, identifiers: &[WalletIdentifier]) -> Self {
        self.info.allowed_identifiers = identifiers.to_vec();
        self
    }
}

fn source_error(status: tonic::Status) -> SourceError {
    let message = status.message().to_string();
    if message.contains("Unexpected EOF decoding stream.") {
        return SourceError::Interrupted(message);
    }
    match status.code() {
        tonic::Code::DeadlineExceeded | tonic::Code::Cancelled => SourceError::Timeout(message),
        tonic::Code::Unavailable => SourceError::Unavailable(message),
        tonic::Code::Unimplemented => SourceError::Unsupported(message),
        tonic::Code::InvalidArgument => SourceError::Rejected(message),
        _ => SourceError::Other(status.to_string()),
    }
}

/// A `SendResponse` with an error code arrives here as `Unknown`.
fn broadcast_error(status: tonic::Status) -> SourceError {
    if status.code() == tonic::Code::Unknown {
        SourceError::Rejected(status.message().to_string())
    } else {
        source_error(status)
    }
}

fn block_id(height: BlockHeight) -> BlockId {
    BlockId {
        height: height.into(),
        hash: vec![],
    }
}

fn block_range(range: std::ops::Range<BlockHeight>) -> BlockRange {
    BlockRange {
        start: Some(block_id(range.start)),
        end: Some(block_id(range.end - 1)),
        pool_types: vec![],
    }
}

fn stream<T: Send + 'static, U: Send + 'static>(
    stream: tonic::Streaming<T>,
    convert: fn(T) -> Result<U, SourceError>,
) -> chain_router::ResponseStream<U> {
    stream
        .map_err(source_error)
        .and_then(move |item| std::future::ready(convert(item)))
        .boxed()
}

fn invalid(what: &str, error: impl std::fmt::Display) -> SourceError {
    SourceError::InvalidData(format!("{what}: {error}"))
}

fn bytes<const N: usize>(what: &str, bytes: &[u8]) -> Result<[u8; N], SourceError> {
    bytes
        .try_into()
        .map_err(|_| invalid(what, format!("expected {N} bytes, found {}", bytes.len())))
}

/// A value of a protocol type, or an error if the bytes are outside its field.
fn element<T>(what: &str, value: CtOption<T>) -> Result<T, SourceError> {
    value
        .into_option()
        .ok_or_else(|| invalid(what, "not a valid field element"))
}

fn height(what: &str, height: u64) -> Result<BlockHeight, SourceError> {
    BlockHeight::try_from(height)
        .map_err(|_| invalid(what, format!("height {height} is too large")))
}

fn tip(tip: BlockId) -> Result<chain::BlockId, SourceError> {
    Ok(chain::BlockId {
        height: height("tip", tip.height)?,
        hash: if tip.hash.is_empty() {
            None
        } else {
            Some(BlockHash(bytes("tip hash", &tip.hash)?))
        },
    })
}

fn compact_block(block: proto::CompactBlock) -> Result<chain::CompactBlock, SourceError> {
    Ok(chain::CompactBlock {
        height: height("block", block.height)?,
        hash: BlockHash(bytes("block hash", &block.hash)?),
        prev_hash: BlockHash(bytes("previous block hash", &block.prev_hash)?),
        time: block.time,
        transactions: block
            .vtx
            .into_iter()
            .map(compact_tx)
            .collect::<Result<_, _>>()?,
        tree_sizes: block.chain_metadata.map(|sizes| chain::TreeSizes {
            sapling: sizes.sapling_commitment_tree_size,
            orchard: sizes.orchard_commitment_tree_size,
            ironwood: sizes.ironwood_commitment_tree_size,
        }),
    })
}

fn compact_output(
    output: proto::CompactSaplingOutput,
) -> Result<CompactOutputDescription, SourceError> {
    Ok(CompactOutputDescription {
        cmu: element(
            "output commitment",
            sapling_crypto::note::ExtractedNoteCommitment::from_bytes(&bytes(
                "output commitment",
                &output.cmu,
            )?),
        )?,
        ephemeral_key: EphemeralKeyBytes(bytes("output ephemeral key", &output.ephemeral_key)?),
        enc_ciphertext: bytes("output ciphertext", &output.ciphertext)?,
    })
}

fn compact_action(action: proto::CompactOrchardAction) -> Result<CompactAction, SourceError> {
    Ok(CompactAction::from_parts(
        element(
            "action nullifier",
            orchard::note::Nullifier::from_bytes(&bytes("action nullifier", &action.nullifier)?),
        )?,
        element(
            "action commitment",
            orchard::note::ExtractedNoteCommitment::from_bytes(&bytes(
                "action commitment",
                &action.cmx,
            )?),
        )?,
        EphemeralKeyBytes(bytes("action ephemeral key", &action.ephemeral_key)?),
        bytes("action ciphertext", &action.ciphertext)?,
    ))
}

/// A transparent output. `Script::read` needs the length of the script before its bytes.
fn transparent_output(output: proto::TxOut) -> Result<TxOut, SourceError> {
    let mut script = Vec::with_capacity(output.script_pub_key.len() + 9);
    CompactSize::write(&mut script, output.script_pub_key.len())
        .map_err(|e| invalid("output script", e))?;
    script.extend_from_slice(&output.script_pub_key);

    Ok(TxOut::new(
        Zatoshis::from_u64(output.value).map_err(|e| invalid("output value", e))?,
        Script::read(&script[..]).map_err(|e| invalid("output script", e))?,
    ))
}

fn compact_tx(tx: proto::CompactTx) -> Result<chain::CompactTx, SourceError> {
    Ok(chain::CompactTx {
        index: tx.index,
        txid: TxId::from_bytes(bytes("txid", &tx.txid)?),
        fee: tx.fee,
        spends: tx
            .spends
            .into_iter()
            .map(|spend| {
                Ok(sapling_crypto::Nullifier(bytes(
                    "spend nullifier",
                    &spend.nf,
                )?))
            })
            .collect::<Result<_, SourceError>>()?,
        outputs: tx
            .outputs
            .into_iter()
            .map(compact_output)
            .collect::<Result<_, _>>()?,
        actions: tx
            .actions
            .into_iter()
            .map(compact_action)
            .collect::<Result<_, _>>()?,
        ironwood_actions: tx
            .ironwood_actions
            .into_iter()
            .map(compact_action)
            .collect::<Result<_, _>>()?,
        transparent_inputs: tx
            .vin
            .into_iter()
            .map(|input| {
                Ok(OutPoint::new(
                    bytes("input txid", &input.prevout_txid)?,
                    input.prevout_index,
                ))
            })
            .collect::<Result<_, SourceError>>()?,
        transparent_outputs: tx
            .vout
            .into_iter()
            .map(transparent_output)
            .collect::<Result<_, _>>()?,
    })
}

/// Reads a commitment tree in the hex form of zcashd and returns its frontier.
fn frontier<H, const DEPTH: u8>(what: &str, tree: &str) -> Result<Frontier<H, DEPTH>, SourceError>
where
    H: HashSer + Hashable + Clone,
{
    let bytes = hex::decode(tree).map_err(|e| invalid(what, e))?;
    if bytes.is_empty() {
        return Ok(Frontier::empty());
    }
    let tree = read_commitment_tree::<H, _, DEPTH>(&bytes[..]).map_err(|e| invalid(what, e))?;
    // `to_frontier` panics on a tree that has a right leaf or a parent and no left leaf.
    let filled = tree.right().is_some() || tree.parents().iter().any(Option::is_some);
    if tree.left().is_none() && filled {
        return Err(invalid(what, "tree has no left leaf"));
    }

    Ok(tree.to_frontier())
}

fn tree_state(state: proto::TreeState) -> Result<chain::TreeState, SourceError> {
    // zcashd prints a block hash in reversed byte order.
    let mut hash = hex::decode(&state.hash).map_err(|e| invalid("tree state hash", e))?;
    hash.reverse();

    Ok(chain::TreeState {
        height: height("tree state", state.height)?,
        hash: BlockHash(bytes("tree state hash", &hash)?),
        time: state.time,
        sapling: frontier("sapling tree", &state.sapling_tree)?,
        orchard: frontier("orchard tree", &state.orchard_tree)?,
        ironwood: frontier("ironwood tree", &state.ironwood_tree)?,
    })
}

fn subtree_root(root: proto::SubtreeRoot) -> Result<chain::SubtreeRoot, SourceError> {
    Ok(chain::SubtreeRoot {
        root: bytes("subtree root", &root.root_hash)?,
        completing_block_hash: BlockHash(bytes("subtree block hash", &root.completing_block_hash)?),
        completing_block_height: height("subtree block", root.completing_block_height)?,
    })
}

/// A height of zero is a mempool transaction. The largest value is a transaction in a block
/// that is not on the main chain.
fn raw_transaction(tx: RawTransaction) -> Result<chain::RawTransaction, SourceError> {
    let mined_height = match tx.height {
        0 | u64::MAX => None,
        mined => Some(height("transaction", mined)?),
    };

    Ok(chain::RawTransaction {
        data: tx.data,
        mined_height,
    })
}

fn server_info(info: proto::LightdInfo) -> Result<chain::ServerInfo, SourceError> {
    Ok(chain::ServerInfo {
        vendor: info.vendor,
        version: info.version,
        chain_name: info.chain_name,
        block_height: height("server tip", info.block_height)?,
        estimated_height: height("estimated tip", info.estimated_height)?,
        sapling_activation_height: height("sapling activation", info.sapling_activation_height)?,
        consensus_branch_id: info.consensus_branch_id,
    })
}

/// zcashd prints a transaction ID in reversed byte order.
fn submitted_txid(hex_txid: String) -> Result<TxId, SourceError> {
    let mut txid = hex::decode(&hex_txid).map_err(|e| invalid("submitted txid", e))?;
    txid.reverse();
    Ok(TxId::from_bytes(bytes("submitted txid", &txid)?))
}

/// The number of a shielded pool in the lightwallet protocol.
fn shielded_protocol(pool: ShieldedPool) -> i32 {
    match pool {
        ShieldedPool::Sapling => 0,
        ShieldedPool::Orchard => 1,
        ShieldedPool::Ironwood => 2,
    }
}

impl ChainSource for GrpcIndexer {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    #[allow(deprecated)]
    fn execute(&self, request: Request) -> BoxFuture<'_, Result<Response, SourceError>> {
        let mut client = self.clone();
        Box::pin(async move {
            let response = match request {
                Request::Info => client
                    .clear_net_client
                    .get_lightd_info(timed(Empty {}, UNARY_TIMEOUT))
                    .await
                    .map(|info| server_info(info.into_inner()).map(Response::Info)),
                Request::Tip => client
                    .clear_net_client
                    .get_latest_block(timed(ChainSpec {}, UNARY_TIMEOUT))
                    .await
                    .map(|block| tip(block.into_inner()).map(Response::Tip)),
                Request::Block(height) => client
                    .get_block(block_id(height), UNARY_TIMEOUT)
                    .await
                    .map(|block| compact_block(block).map(Response::Block)),
                Request::Blocks(range) => client
                    .get_block_range(block_range(range), HEAVY_TIMEOUT)
                    .await
                    .map(|blocks| Ok(Response::Blocks(stream(blocks, compact_block)))),
                Request::Nullifiers(range) => client
                    .get_block_range_nullifiers(block_range(range), HEAVY_TIMEOUT)
                    .await
                    .map(|blocks| Ok(Response::Blocks(stream(blocks, compact_block)))),
                Request::TreeState(height) => client
                    .get_tree_state(block_id(height), UNARY_TIMEOUT)
                    .await
                    .map(|state| tree_state(state).map(Response::TreeState)),
                Request::SubtreeRoots {
                    pool,
                    start_index,
                    max_entries,
                } => client
                    .get_subtree_roots(
                        GetSubtreeRootsArg {
                            start_index,
                            shielded_protocol: shielded_protocol(pool),
                            max_entries,
                        },
                        HEAVY_TIMEOUT,
                    )
                    .await
                    .map(|roots| Ok(Response::SubtreeRoots(stream(roots, subtree_root)))),
                Request::Transaction(txid) => client
                    .get_transaction(
                        TxFilter {
                            block: None,
                            index: 0,
                            hash: txid.as_ref().to_vec(),
                        },
                        UNARY_TIMEOUT,
                    )
                    .await
                    .map(|tx| raw_transaction(tx).map(Response::Transaction)),
                #[cfg(feature = "globally-public-transparent")]
                Request::AddressTransactions { address, range } => {
                    use crate::TransparentIndexer as _;
                    use lightwallet_protocol::TransparentAddressBlockFilter;

                    client
                        .get_taddress_txids(
                            TransparentAddressBlockFilter {
                                address,
                                range: Some(block_range(range)),
                            },
                            HEAVY_TIMEOUT,
                        )
                        .await
                        .map(|txs| Ok(Response::Transactions(stream(txs, raw_transaction))))
                }
                Request::Mempool => client
                    .get_mempool_stream(HEAVY_TIMEOUT)
                    .await
                    .map(|txs| Ok(Response::Transactions(stream(txs, raw_transaction)))),
                Request::Broadcast(data) => {
                    return client
                        .send_transaction(RawTransaction { data, height: 0 }, UNARY_TIMEOUT)
                        .await
                        .map_err(broadcast_error)
                        .and_then(|txid| submitted_txid(txid).map(Response::Broadcast));
                }
                unsupported => return Err(SourceError::Unsupported(format!("{unsupported:?}"))),
            };

            response.map_err(source_error)?
        })
    }
}

fn timed<T>(message: T, timeout: Duration) -> tonic::Request<T> {
    let mut request = tonic::Request::new(message);
    request.set_timeout(timeout);
    request
}

pub(crate) fn info(uri: &http::Uri, transport: TransportInfo) -> SourceInfo {
    let mut info = SourceInfo::new(uri.to_string(), Integrity::Unverified, transport);
    info.capabilities.address_queries = cfg!(feature = "globally-public-transparent");
    info
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_codes_map_to_source_errors() {
        let timeout = source_error(tonic::Status::deadline_exceeded("slow"));
        assert_eq!(timeout, SourceError::Timeout("slow".into()));
        let unsupported = source_error(tonic::Status::unimplemented("no"));
        assert_eq!(unsupported, SourceError::Unsupported("no".into()));
        let interrupted = source_error(tonic::Status::internal("Unexpected EOF decoding stream."));
        assert!(matches!(interrupted, SourceError::Interrupted(_)));
    }

    #[test]
    fn only_a_send_response_error_is_a_rejected_broadcast() {
        let refused = broadcast_error(tonic::Status::unknown("bad-txns"));
        assert_eq!(refused, SourceError::Rejected("bad-txns".into()));

        let broken = broadcast_error(tonic::Status::internal("database is down"));
        assert!(matches!(broken, SourceError::Other(_)));
        let offline = broadcast_error(tonic::Status::unavailable("no route"));
        assert!(matches!(offline, SourceError::Unavailable(_)));
    }
}

#[cfg(test)]
mod conversion_tests {
    use super::*;

    fn node(byte: u8) -> String {
        format!("01{}", hex::encode([byte; 32]))
    }

    fn sapling(tree: &str) -> Result<chain::SaplingFrontier, SourceError> {
        frontier("tree", tree)
    }

    fn nodes(bytes: &[u8]) -> Vec<sapling_crypto::Node> {
        bytes
            .iter()
            .map(|byte| sapling_crypto::Node::from_bytes([*byte; 32]).unwrap())
            .collect()
    }

    #[test]
    fn frontier_of_an_empty_tree_is_empty() {
        assert_eq!(sapling("").unwrap(), Frontier::empty());
        assert_eq!(sapling("000000").unwrap(), Frontier::empty());
    }

    #[test]
    fn frontier_takes_the_last_leaf_and_the_filled_parents() {
        let one_leaf = sapling(&format!("{}0000", node(1))).unwrap();
        let one_leaf = one_leaf.value().unwrap();
        assert_eq!(u64::from(one_leaf.position()), 0);
        assert_eq!(one_leaf.leaf(), &nodes(&[1])[0]);
        assert!(one_leaf.ommers().is_empty());

        let tree = format!("{}{}02{}{}", node(1), node(2), "00", node(3));
        let six_leaves = sapling(&tree).unwrap();
        let six_leaves = six_leaves.value().unwrap();
        assert_eq!(u64::from(six_leaves.position()), 5);
        assert_eq!(six_leaves.leaf(), &nodes(&[2])[0]);
        assert_eq!(six_leaves.ommers(), nodes(&[1, 3]));
    }

    #[test]
    fn frontier_rejects_a_damaged_tree() {
        let no_left_leaf = format!("00{}00", node(2));
        let parent_and_no_leaf = format!("000001{}", node(3));
        let no_parent_count = format!("{}00", node(1));
        let node_outside_the_field = format!("{}0000", node(0xff));
        for tree in [
            "zz",
            "01ab",
            "0000ff",
            &no_left_leaf,
            &parent_and_no_leaf,
            &no_parent_count,
            &node_outside_the_field,
        ] {
            assert!(
                matches!(sapling(tree), Err(SourceError::InvalidData(_))),
                "{tree}"
            );
        }
    }

    #[test]
    fn compact_block_needs_hashes_of_32_bytes() {
        let block = proto::CompactBlock {
            height: 5,
            hash: vec![1; 32],
            prev_hash: vec![2; 32],
            ..Default::default()
        };
        let converted = compact_block(block.clone()).unwrap();
        assert_eq!(converted.height, BlockHeight::from_u32(5));
        assert_eq!(converted.prev_hash, BlockHash([2; 32]));

        let short = proto::CompactBlock {
            hash: vec![1; 31],
            ..block
        };
        assert!(matches!(
            compact_block(short),
            Err(SourceError::InvalidData(_))
        ));
    }

    #[test]
    fn output_with_a_short_ciphertext_is_invalid() {
        let output = |ciphertext| proto::CompactTx {
            txid: vec![0; 32],
            outputs: vec![proto::CompactSaplingOutput {
                cmu: vec![0; 32],
                ephemeral_key: vec![0; 32],
                ciphertext,
            }],
            ..Default::default()
        };

        assert!(compact_tx(output(vec![0; 52])).is_ok());
        assert!(matches!(
            compact_tx(output(vec![0; 51])),
            Err(SourceError::InvalidData(_))
        ));
    }

    #[test]
    fn commitment_outside_the_field_is_invalid() {
        let output = |cmu| proto::CompactSaplingOutput {
            cmu,
            ephemeral_key: vec![0; 32],
            ciphertext: vec![0; 52],
        };

        assert!(compact_output(output(vec![0; 32])).is_ok());
        assert!(matches!(
            compact_output(output(vec![0xff; 32])),
            Err(SourceError::InvalidData(_))
        ));
    }

    #[test]
    fn transaction_height_zero_or_maximum_is_not_mined() {
        let mined = |height| {
            raw_transaction(RawTransaction {
                data: vec![],
                height,
            })
            .unwrap()
            .mined_height
        };

        assert_eq!(mined(0), None);
        assert_eq!(mined(u64::MAX), None);
        assert_eq!(mined(7), Some(BlockHeight::from_u32(7)));
    }

    #[test]
    fn submitted_txid_is_hex_in_reversed_byte_order() {
        let mut bytes = [0u8; 32];
        bytes[0] = 0xab;
        let hex_txid = format!("{}ab", "00".repeat(31));

        assert_eq!(submitted_txid(hex_txid).unwrap(), TxId::from_bytes(bytes));
        assert!(matches!(
            submitted_txid("abcd".into()),
            Err(SourceError::InvalidData(_))
        ));
    }
}
