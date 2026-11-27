//! The set of configured sources and the execution of routes.

use std::{ops::Range, sync::Arc};

use futures::{StreamExt, TryStreamExt, future::join_all, stream};
use zcash_protocol::{ShieldedPool, TxId, consensus::BlockHeight};

use crate::{
    ChainSource, Failover, Integrity, Request, Response, ResponseStream, Route, Router,
    SourceError, SourceId, SourceInfo, TransportInfo,
    chain::{BlockId, CompactBlock, RawTransaction, ServerInfo, SubtreeRoot, TreeState},
};

/// Why the caller can accept a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum IntegrityBasis {
    /// The result is from a source with assumed integrity.
    Assumed,
    /// This number of sources returned the same result.
    Quorum(usize),
    /// The result is from one unverified source.
    Unverified,
}

/// A result and where it came from.
#[derive(Debug, Clone)]
pub struct Fetched<T> {
    /// The result.
    pub value: T,
    /// Its source.
    pub source: SourceId,
    /// Why it can be accepted.
    pub basis: IntegrityBasis,
}

/// What a request will get from the current route.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct RouteInfo {
    /// Expected integrity basis of the result.
    pub basis: IntegrityBasis,
    /// The weakest transport properties on the route.
    pub transport: TransportInfo,
}

macro_rules! typed {
    ($fetched:expr, $variant:ident) => {{
        let Fetched {
            value,
            source,
            basis,
        } = $fetched;
        match value {
            Response::$variant(value) => Ok(Fetched {
                value,
                source,
                basis,
            }),
            _ => Err(SourceError::UnexpectedResponse),
        }
    }};
}

/// The configured sources and the router that selects among them.
pub struct SourceSet {
    sources: Vec<Arc<dyn ChainSource>>,
    infos: Vec<SourceInfo>,
    router: Arc<dyn Router>,
}

impl SourceSet {
    /// Creates a set. A [`SourceId`] is the position of a source in `sources`.
    pub fn new(sources: Vec<Arc<dyn ChainSource>>, router: Arc<dyn Router>) -> Self {
        let infos = sources.iter().map(|source| source.info().clone()).collect();
        Self {
            sources,
            infos,
            router,
        }
    }

    /// A set of one source with a [`Failover`] router.
    pub fn single(source: impl ChainSource + 'static) -> Self {
        Self::new(vec![Arc::new(source)], Arc::new(Failover::default()))
    }

    /// Information on each source, indexed by [`SourceId`].
    pub fn sources(&self) -> &[SourceInfo] {
        &self.infos
    }

    /// Passes a failure of `source` to the router.
    pub fn on_failure(&self, source: SourceId, error: &SourceError) {
        self.router.on_failure(source, error);
    }

    /// What `request` will get from the current route.
    pub fn route_info(&self, request: &Request) -> Result<RouteInfo, SourceError> {
        let (ids, quorum) = match self.router.select_route(request, &self.infos) {
            Route::Failover(ids) => (ids.into_iter().take(1).collect(), None),
            Route::Quorum { of, need } => (of, Some(need)),
        };
        let infos: Vec<_> = ids.iter().filter_map(|id| self.infos.get(id.0)).collect();
        if infos.is_empty() {
            return Err(SourceError::NoSource);
        }
        let any_assumed = infos
            .iter()
            .any(|info| info.integrity == Integrity::Assumed);
        let all_assumed = infos
            .iter()
            .all(|info| info.integrity == Integrity::Assumed);
        let basis = match quorum {
            Some(_) if any_assumed => IntegrityBasis::Assumed,
            Some(need) => IntegrityBasis::Quorum(need),
            None if all_assumed => IntegrityBasis::Assumed,
            None => IntegrityBasis::Unverified,
        };
        let transport = infos[1..].iter().fold(infos[0].transport, |weakest, info| {
            weakest.weakest(info.transport)
        });

        Ok(RouteInfo { basis, transport })
    }

    /// Sends `request` on the route that the router selects.
    pub async fn send(&self, request: Request) -> Result<Fetched<Response>, SourceError> {
        match self.router.select_route(&request, &self.infos) {
            Route::Failover(ids) => self.failover(request, ids).await,
            Route::Quorum { of, need } => self.quorum(request, of, need.max(1)).await,
        }
    }

    /// See [`Request::Info`].
    pub async fn info(&self) -> Result<Fetched<ServerInfo>, SourceError> {
        typed!(self.send(Request::Info).await?, Info)
    }

    /// See [`Request::Tip`].
    pub async fn tip(&self) -> Result<Fetched<BlockId>, SourceError> {
        typed!(self.send(Request::Tip).await?, Tip)
    }

    /// See [`Request::Block`].
    pub async fn block(&self, height: BlockHeight) -> Result<Fetched<CompactBlock>, SourceError> {
        typed!(self.send(Request::Block(height)).await?, Block)
    }

    /// See [`Request::Blocks`].
    pub async fn blocks(
        &self,
        range: Range<BlockHeight>,
    ) -> Result<Fetched<ResponseStream<CompactBlock>>, SourceError> {
        typed!(self.send(Request::Blocks(range)).await?, Blocks)
    }

    /// See [`Request::Nullifiers`].
    pub async fn nullifiers(
        &self,
        range: Range<BlockHeight>,
    ) -> Result<Fetched<ResponseStream<CompactBlock>>, SourceError> {
        typed!(self.send(Request::Nullifiers(range)).await?, Blocks)
    }

    /// See [`Request::TreeState`].
    pub async fn tree_state(&self, height: BlockHeight) -> Result<Fetched<TreeState>, SourceError> {
        typed!(self.send(Request::TreeState(height)).await?, TreeState)
    }

    /// See [`Request::SubtreeRoots`].
    pub async fn subtree_roots(
        &self,
        pool: ShieldedPool,
        start_index: u32,
        max_entries: u32,
    ) -> Result<Fetched<ResponseStream<SubtreeRoot>>, SourceError> {
        let request = Request::SubtreeRoots {
            pool,
            start_index,
            max_entries,
        };
        typed!(self.send(request).await?, SubtreeRoots)
    }

    /// See [`Request::Transaction`].
    pub async fn transaction(&self, txid: TxId) -> Result<Fetched<RawTransaction>, SourceError> {
        typed!(self.send(Request::Transaction(txid)).await?, Transaction)
    }

    /// See [`Request::AddressTransactions`].
    pub async fn address_transactions(
        &self,
        address: String,
        range: Range<BlockHeight>,
    ) -> Result<Fetched<ResponseStream<RawTransaction>>, SourceError> {
        let request = Request::AddressTransactions { address, range };
        typed!(self.send(request).await?, Transactions)
    }

    /// See [`Request::Mempool`].
    pub async fn mempool(&self) -> Result<Fetched<ResponseStream<RawTransaction>>, SourceError> {
        typed!(self.send(Request::Mempool).await?, Transactions)
    }

    /// See [`Request::Broadcast`]. Returns the transaction ID from the source.
    pub async fn submit_transaction(
        &self,
        transaction: Vec<u8>,
    ) -> Result<Fetched<TxId>, SourceError> {
        typed!(self.send(Request::Broadcast(transaction)).await?, Broadcast)
    }

    fn basis(&self, source: SourceId) -> IntegrityBasis {
        match self.infos[source.0].integrity {
            Integrity::Assumed => IntegrityBasis::Assumed,
            Integrity::Unverified => IntegrityBasis::Unverified,
        }
    }

    async fn send_to(&self, id: SourceId, request: Request) -> Result<Response, SourceError> {
        let source = self.sources.get(id.0).ok_or(SourceError::NoSource)?;
        match source.execute(request).await {
            Ok(response) => {
                self.router.on_success(id);
                Ok(match response {
                    Response::Blocks(blocks) => Response::Blocks(self.watch(id, blocks)),
                    Response::SubtreeRoots(roots) => Response::SubtreeRoots(self.watch(id, roots)),
                    Response::Transactions(txs) => Response::Transactions(self.watch(id, txs)),
                    unary => unary,
                })
            }
            Err(e) => {
                self.on_failure(id, &e);
                Err(e)
            }
        }
    }

    fn watch<T: 'static>(&self, id: SourceId, stream: ResponseStream<T>) -> ResponseStream<T> {
        let router = self.router.clone();
        stream
            .inspect_err(move |e| {
                router.on_failure(id, e);
            })
            .boxed()
    }

    async fn failover(
        &self,
        request: Request,
        ids: Vec<SourceId>,
    ) -> Result<Fetched<Response>, SourceError> {
        let mut error = SourceError::NoSource;
        for source in ids {
            match self.send_to(source, request.clone()).await {
                Ok(value) => {
                    return Ok(Fetched {
                        value,
                        source,
                        basis: self.basis(source),
                    });
                }
                Err(e)
                    if matches!(request, Request::Broadcast(_))
                        && !matches!(e, SourceError::Unavailable(_)) =>
                {
                    return Err(e);
                }
                Err(e) => error = e,
            }
        }

        Err(error)
    }

    async fn quorum(
        &self,
        request: Request,
        of: Vec<SourceId>,
        need: usize,
    ) -> Result<Fetched<Response>, SourceError> {
        let replies = join_all(of.iter().map(|&id| {
            let request = request.clone();
            async move {
                match self.send_to(id, request).await {
                    Ok(response) => Collected::from_response(response).await,
                    Err(e) => Err(e),
                }
            }
        }))
        .await;

        let mut error = SourceError::NoSource;
        let mut responses = Vec::new();
        for (id, reply) in of.into_iter().zip(replies) {
            match reply {
                Ok(response) => responses.push((id, response)),
                Err(e) => error = e,
            }
        }

        let assumed = responses
            .iter()
            .position(|(id, _)| self.infos[id.0].integrity == Integrity::Assumed);
        let (source, value, basis) = if let Some(position) = assumed {
            let (source, value) = responses.swap_remove(position);
            (source, value, IntegrityBasis::Assumed)
        } else if responses.len() < need {
            return Err(error);
        } else {
            let (source, value) = agreed(&responses, need).ok_or(SourceError::Disagreement)?;
            (source, value, IntegrityBasis::Quorum(need))
        };

        for (id, response) in &responses {
            if value.conflicts(response) {
                self.on_failure(*id, &SourceError::FailedVerification);
            }
        }

        Ok(Fetched {
            value: value.into_response(),
            source,
            basis,
        })
    }
}

/// The response that `need` sources returned.
///
/// Sources can be a block apart, so a tip or a list of subtree roots only needs support up
/// to what `need` sources have reached.
fn agreed(responses: &[(SourceId, Collected)], need: usize) -> Option<(SourceId, Collected)> {
    let mut sizes: Vec<_> = responses.iter().filter_map(|(_, a)| a.size()).collect();
    sizes.sort_unstable_by(|a, b| b.cmp(a));
    let limit = sizes.get(need - 1).copied();

    responses
        .iter()
        .find(|(_, response)| {
            response.size() == limit
                && responses
                    .iter()
                    .filter(|(_, other)| other.size() >= limit && !response.conflicts(other))
                    .count()
                    >= need
        })
        .cloned()
}

#[derive(Clone, PartialEq)]
enum Collected {
    Info(ServerInfo),
    Tip(BlockId),
    Block(CompactBlock),
    Blocks(Vec<CompactBlock>),
    TreeState(TreeState),
    SubtreeRoots(Vec<SubtreeRoot>),
    Transaction(RawTransaction),
    Transactions(Vec<RawTransaction>),
    Broadcast(TxId),
}

impl Collected {
    async fn from_response(response: Response) -> Result<Self, SourceError> {
        Ok(match response {
            Response::Info(info) => Collected::Info(info),
            Response::Tip(tip) => Collected::Tip(tip),
            Response::Block(block) => Collected::Block(block),
            Response::Blocks(blocks) => Collected::Blocks(blocks.try_collect().await?),
            Response::TreeState(tree_state) => Collected::TreeState(tree_state),
            Response::SubtreeRoots(roots) => Collected::SubtreeRoots(roots.try_collect().await?),
            Response::Transaction(tx) => Collected::Transaction(tx),
            Response::Transactions(txs) => Collected::Transactions(txs.try_collect().await?),
            Response::Broadcast(txid) => Collected::Broadcast(txid),
        })
    }

    fn into_response(self) -> Response {
        fn restream<T: Send + 'static>(items: Vec<T>) -> ResponseStream<T> {
            stream::iter(items.into_iter().map(Ok)).boxed()
        }

        match self {
            Collected::Info(info) => Response::Info(info),
            Collected::Tip(tip) => Response::Tip(tip),
            Collected::Block(block) => Response::Block(block),
            Collected::Blocks(blocks) => Response::Blocks(restream(blocks)),
            Collected::TreeState(tree_state) => Response::TreeState(tree_state),
            Collected::SubtreeRoots(roots) => Response::SubtreeRoots(restream(roots)),
            Collected::Transaction(tx) => Response::Transaction(tx),
            Collected::Transactions(txs) => Response::Transactions(restream(txs)),
            Collected::Broadcast(txid) => Response::Broadcast(txid),
        }
    }

    /// Chain progress of an response that grows with the chain.
    fn size(&self) -> Option<u64> {
        match self {
            Collected::Tip(tip) => Some(u64::from(u32::from(tip.height))),
            Collected::SubtreeRoots(roots) => Some(roots.len() as u64),
            _ => None,
        }
    }

    fn conflicts(&self, other: &Self) -> bool {
        match (self, other) {
            (Collected::Tip(_), Collected::Tip(_)) => false,
            (Collected::SubtreeRoots(a), Collected::SubtreeRoots(b)) => {
                a.iter().zip(b).any(|(a, b)| a != b)
            }
            _ => self != other,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use futures::future::BoxFuture;

    use super::*;
    use crate::{
        Bandwidth, Quorum, SourceExposure,
        chain::{BlockHash, TreeNode},
    };

    struct Mock {
        info: SourceInfo,
        tip: u32,
        tree: &'static str,
        roots: &'static [u8],
        fails: bool,
        submit_error: Option<SourceError>,
        breaks_streams: bool,
        wrong_kind: bool,
        calls: AtomicUsize,
    }

    fn mock(integrity: Integrity, tip: u32, tree: &'static str) -> Arc<Mock> {
        Arc::new(Mock {
            info: SourceInfo::new("mock", integrity, TransportInfo::direct()),
            tip,
            tree,
            roots: &[],
            fails: false,
            submit_error: None,
            breaks_streams: false,
            wrong_kind: false,
            calls: AtomicUsize::new(0),
        })
    }

    fn custom(change: impl FnOnce(&mut Mock)) -> Arc<Mock> {
        let mut source = mock(UNVERIFIED, 10, "a");
        change(Arc::get_mut(&mut source).unwrap());
        source
    }

    fn with_roots(roots: &'static [u8]) -> Arc<Mock> {
        custom(|source| source.roots = roots)
    }

    fn failing() -> Arc<Mock> {
        custom(|source| source.fails = true)
    }

    impl ChainSource for Mock {
        fn info(&self) -> &SourceInfo {
            &self.info
        }

        fn execute(&self, request: Request) -> BoxFuture<'_, Result<Response, SourceError>> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            Box::pin(async move {
                if self.fails {
                    return Err(SourceError::Unavailable("down".into()));
                }
                if self.wrong_kind {
                    return Ok(Response::Broadcast(TxId::from_bytes([0; 32])));
                }
                Ok(match request {
                    Request::Tip => Response::Tip(BlockId {
                        height: BlockHeight::from_u32(self.tip),
                        hash: None,
                    }),
                    Request::TreeState(_) => Response::TreeState(TreeState {
                        height: HEIGHT,
                        hash: marked(self.tree),
                        time: 0,
                        sapling: None,
                        orchard: None,
                        ironwood: None,
                    }),
                    Request::Blocks(_) if self.breaks_streams => {
                        let cut = Err(SourceError::Interrupted("cut".into()));
                        Response::Blocks(stream::iter([cut]).boxed())
                    }
                    Request::Blocks(_) => Response::Blocks(stream::empty().boxed()),
                    Request::Transaction(_) => Response::Transaction(RawTransaction {
                        data: Vec::new(),
                        mined_height: None,
                    }),
                    Request::Broadcast(_) => match &self.submit_error {
                        Some(error) => return Err(error.clone()),
                        None => Response::Broadcast(TxId::from_bytes([0; 32])),
                    },
                    Request::SubtreeRoots { .. } => {
                        let roots = self.roots.iter().map(|root| {
                            Ok(SubtreeRoot {
                                root: TreeNode::from([*root; 32]),
                                completing_block_hash: BlockHash::default(),
                                completing_block_height: HEIGHT,
                            })
                        });
                        Response::SubtreeRoots(stream::iter(roots).boxed())
                    }
                    _ => return Err(SourceError::Unsupported("mock".into())),
                })
            })
        }
    }

    struct AskAll;

    impl Router for AskAll {
        fn select_route(&self, _request: &Request, sources: &[SourceInfo]) -> Route {
            Route::Quorum {
                of: (0..sources.len()).map(SourceId).collect(),
                need: 2,
            }
        }
    }

    fn set(sources: &[&Arc<Mock>], router: impl Router + 'static) -> SourceSet {
        SourceSet::new(
            sources
                .iter()
                .map(|source| Arc::clone(source) as Arc<dyn ChainSource>)
                .collect(),
            Arc::new(router),
        )
    }

    const UNVERIFIED: Integrity = Integrity::Unverified;

    /// A block hash that stands for one version of a tree state.
    fn marked(tree: &str) -> BlockHash {
        BlockHash::from([tree.as_bytes()[0]; 32])
    }
    const HEIGHT: BlockHeight = BlockHeight::from_u32(1);

    #[tokio::test]
    async fn failover_uses_the_next_source_and_remembers_the_failure() {
        let (down, up) = (failing(), mock(UNVERIFIED, 10, "a"));
        let sources = set(&[&down, &up], Failover::default());

        let tip = sources.tip().await.unwrap();
        assert_eq!(tip.source, SourceId(1));
        assert_eq!(tip.basis, IntegrityBasis::Unverified);

        sources.tip().await.unwrap();
        assert_eq!(down.calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn failover_returns_the_last_error_when_every_source_fails() {
        let sources = set(&[&failing(), &failing()], Failover::default());

        assert_eq!(
            sources.tip().await.unwrap_err(),
            SourceError::Unavailable("down".into())
        );
    }

    #[tokio::test]
    async fn source_without_the_capability_gets_no_request() {
        let (limited, full) = (
            custom(|source| source.info.capabilities.subtree_roots = false),
            with_roots(&[1]),
        );

        let sources = set(&[&limited, &full], Failover::default());
        assert_eq!(
            sources
                .subtree_roots(ShieldedPool::Sapling, 0, 0)
                .await
                .unwrap()
                .source,
            SourceId(1)
        );
        assert_eq!(limited.calls.load(Ordering::Relaxed), 0);

        let sources = set(&[&limited], Failover::default());
        assert!(matches!(
            sources.subtree_roots(ShieldedPool::Sapling, 0, 0).await,
            Err(SourceError::NoSource)
        ));
    }

    fn exposed_as(exposure: SourceExposure) -> Arc<Mock> {
        custom(|source| source.info.transport.exposed_to_source = exposure)
    }

    #[tokio::test]
    async fn disclosing_request_goes_to_the_least_exposed_source() {
        let (direct, pseudonym, unlinkable) = (
            exposed_as(SourceExposure::Address),
            exposed_as(SourceExposure::Pseudonym),
            exposed_as(SourceExposure::Nothing),
        );
        let txid = TxId::from_bytes([0; 32]);

        let sources = set(&[&direct, &pseudonym, &unlinkable], Failover::default());
        assert_eq!(sources.transaction(txid).await.unwrap().source, SourceId(2));
        assert_eq!(sources.tip().await.unwrap().source, SourceId(0));

        let sources = set(&[&direct, &pseudonym], Failover::default());
        assert_eq!(sources.transaction(txid).await.unwrap().source, SourceId(1));
    }

    #[tokio::test]
    async fn route_info_gives_the_weakest_transport_on_the_route() {
        let (direct, private) = (
            mock(UNVERIFIED, 10, "a"),
            custom(|source| {
                let transport = &mut source.info.transport;
                transport.exposed_to_source = SourceExposure::Nothing;
                transport.exposed_to_network.destination = false;
                transport.metered = true;
                transport.bandwidth = Bandwidth::Low;
            }),
        );
        let sources = set(&[&direct, &private], AskAll);

        let transport = sources.route_info(&Request::Tip).unwrap().transport;
        assert_eq!(transport.exposed_to_source, SourceExposure::Address);
        assert!(transport.exposed_to_network.destination);
        assert!(transport.metered);
        assert_eq!(transport.bandwidth, Bandwidth::Low);
    }

    #[tokio::test]
    async fn transaction_goes_to_the_next_source_only_if_the_first_is_unreachable() {
        for error in [
            SourceError::Rejected("invalid".into()),
            SourceError::Other("internal".into()),
            SourceError::Timeout("slow".into()),
        ] {
            let (first, next) = (
                custom(|source| source.submit_error = Some(error.clone())),
                mock(UNVERIFIED, 10, "a"),
            );
            let sources = set(&[&first, &next], Failover::default());

            assert_eq!(
                sources.submit_transaction(vec![1]).await.unwrap_err(),
                error
            );
            assert_eq!(next.calls.load(Ordering::Relaxed), 0);
        }

        let (unreachable, next) = (
            custom(|source| {
                source.submit_error = Some(SourceError::Unavailable("no route".into()));
            }),
            mock(UNVERIFIED, 10, "a"),
        );
        let sources = set(&[&unreachable, &next], Failover::default());

        assert_eq!(
            sources.submit_transaction(vec![1]).await.unwrap().source,
            SourceId(1)
        );
    }

    #[tokio::test]
    async fn stream_error_moves_the_next_request_to_another_source() {
        let (broken, good) = (
            custom(|source| source.breaks_streams = true),
            mock(UNVERIFIED, 10, "a"),
        );
        let sources = set(&[&broken, &good], Failover::default());
        let range = HEIGHT..HEIGHT + 5;

        let blocks = sources.blocks(range.clone()).await.unwrap();
        assert_eq!(blocks.source, SourceId(0));
        assert!(blocks.value.try_collect::<Vec<_>>().await.is_err());

        assert_eq!(sources.blocks(range).await.unwrap().source, SourceId(1));
    }

    #[tokio::test]
    async fn response_of_the_wrong_kind_is_an_error() {
        let sources = set(
            &[&custom(|source| source.wrong_kind = true)],
            Failover::default(),
        );

        assert_eq!(
            sources.tip().await.unwrap_err(),
            SourceError::UnexpectedResponse
        );
    }

    #[tokio::test]
    async fn quorum_returns_the_majority_response_and_drops_the_other_source() {
        let (a, liar, c) = (
            mock(UNVERIFIED, 10, "a"),
            mock(UNVERIFIED, 10, "b"),
            mock(UNVERIFIED, 10, "a"),
        );
        let sources = set(&[&a, &liar, &c], Quorum::new(2));

        let tree_state = sources.tree_state(HEIGHT).await.unwrap();
        assert_eq!(tree_state.value.hash, marked("a"));
        assert_eq!(tree_state.basis, IntegrityBasis::Quorum(2));

        sources.tree_state(HEIGHT).await.unwrap();
        assert_eq!(liar.calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn quorum_fails_without_enough_equal_responses() {
        let (a, b) = (mock(UNVERIFIED, 10, "a"), mock(UNVERIFIED, 10, "b"));
        let sources = set(&[&a, &b], Quorum::new(2));

        assert_eq!(
            sources.tree_state(HEIGHT).await.unwrap_err(),
            SourceError::Disagreement
        );
    }

    #[tokio::test]
    async fn quorum_fails_with_too_few_responses() {
        let (a, down) = (mock(UNVERIFIED, 10, "a"), failing());
        let sources = set(&[&a, &down], Quorum::new(2));

        assert_eq!(
            sources.tree_state(HEIGHT).await.unwrap_err(),
            SourceError::Unavailable("down".into())
        );
    }

    #[tokio::test]
    async fn quorum_tip_is_the_height_that_enough_sources_reached() {
        let (a, b, c) = (
            mock(UNVERIFIED, 12, "a"),
            mock(UNVERIFIED, 11, "a"),
            mock(UNVERIFIED, 10, "a"),
        );
        let sources = set(&[&a, &b, &c], Quorum::new(2));

        assert_eq!(
            sources.tip().await.unwrap().value.height,
            BlockHeight::from_u32(11)
        );
    }

    #[tokio::test]
    async fn quorum_subtree_roots_stop_at_the_last_common_root() {
        let (ahead, behind, liar) = (
            with_roots(&[1, 2, 3]),
            with_roots(&[1, 2]),
            with_roots(&[9, 9]),
        );
        let sources = set(&[&ahead, &behind, &liar], Quorum::new(2));

        let roots = sources
            .subtree_roots(ShieldedPool::Sapling, 0, 0)
            .await
            .unwrap();
        let roots: Vec<_> = roots.value.try_collect().await.unwrap();
        assert_eq!(
            roots
                .iter()
                .map(|root| root.root.as_bytes()[0])
                .collect::<Vec<_>>(),
            [1, 2]
        );

        sources
            .subtree_roots(ShieldedPool::Sapling, 0, 0)
            .await
            .unwrap();
        assert_eq!(ahead.calls.load(Ordering::Relaxed), 2);
        assert_eq!(liar.calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn assumed_source_overrules_the_other_sources() {
        let (a, b, assumed) = (
            mock(UNVERIFIED, 10, "a"),
            mock(UNVERIFIED, 10, "a"),
            mock(Integrity::Assumed, 10, "b"),
        );
        let sources = set(&[&a, &b, &assumed], AskAll);

        let tree_state = sources.tree_state(HEIGHT).await.unwrap();
        assert_eq!(tree_state.value.hash, marked("b"));
        assert_eq!(tree_state.basis, IntegrityBasis::Assumed);
    }

    #[tokio::test]
    async fn quorum_router_prefers_an_assumed_source() {
        let (a, assumed) = (mock(UNVERIFIED, 10, "a"), mock(Integrity::Assumed, 10, "b"));
        let sources = set(&[&a, &assumed], Quorum::new(2));

        let tree_state = sources.tree_state(HEIGHT).await.unwrap();
        assert_eq!(tree_state.basis, IntegrityBasis::Assumed);
        assert_eq!(a.calls.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn quorum_router_sends_block_ranges_to_one_source() {
        let (a, b) = (mock(UNVERIFIED, 10, "a"), mock(UNVERIFIED, 10, "a"));
        let sources = set(&[&a, &b], Quorum::new(2));

        let range = HEIGHT..HEIGHT + 5;
        let info = sources.route_info(&Request::Blocks(range.clone())).unwrap();
        assert_eq!(info.basis, IntegrityBasis::Unverified);
        assert_eq!(
            sources.route_info(&Request::Tip).unwrap().basis,
            IntegrityBasis::Quorum(2)
        );

        sources.blocks(range).await.unwrap();
        assert_eq!(b.calls.load(Ordering::Relaxed), 0);
    }
}
