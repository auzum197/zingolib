//! Sync strategies.

use std::sync::Arc;

use futures::future::BoxFuture;
use tokio::sync::{RwLock, watch};
use wallet_api::SourceSet;
use zcash_protocol::consensus;

use crate::{
    client::Policy,
    config::SyncConfig,
    error::SyncError,
    events::SyncEmitter,
    sync::{SyncResult, sync_with_policy},
    wallet::{
        SyncMode,
        traits::{
            SyncBlocks, SyncNullifiers, SyncOutPoints, SyncShardTrees, SyncTransactions, SyncWallet,
        },
    },
};

/// What one sync run works with.
#[non_exhaustive]
pub struct SyncContext<P, W> {
    /// Where chain data comes from.
    pub sources: Arc<SourceSet>,
    /// Consensus parameters of the chain.
    pub consensus_parameters: P,
    /// The wallet to sync.
    pub wallet: Arc<RwLock<W>>,
    /// Controls the run and publishes its state.
    pub sync_mode: watch::Sender<SyncMode>,
    /// Sync configuration.
    pub config: SyncConfig,
    /// Announces sync events to subscribers.
    pub events: SyncEmitter,
}

impl<P, W> SyncContext<P, W> {
    /// Creates a context from its parts.
    pub fn new(
        sources: Arc<SourceSet>,
        consensus_parameters: P,
        wallet: Arc<RwLock<W>>,
        sync_mode: watch::Sender<SyncMode>,
        config: SyncConfig,
        events: SyncEmitter,
    ) -> Self {
        Self {
            sources,
            consensus_parameters,
            wallet,
            sync_mode,
            config,
            events,
        }
    }
}

/// A way to sync a wallet to the chain.
///
/// The contract for `sync_mode` is the one of [`crate::sync()`].
pub trait SyncStrategy<P, W: SyncWallet>: Send + Sync {
    /// Syncs the wallet to the chain tip.
    fn run(
        &self,
        context: SyncContext<P, W>,
    ) -> BoxFuture<'static, Result<SyncResult, SyncError<W::Error>>>;
}

fn run<P, W>(
    context: SyncContext<P, W>,
    policy: Policy,
) -> BoxFuture<'static, Result<SyncResult, SyncError<W::Error>>>
where
    P: consensus::Parameters + Sync + Send + 'static,
    W: SyncWallet
        + SyncBlocks
        + SyncTransactions
        + SyncNullifiers
        + SyncOutPoints
        + SyncShardTrees
        + Send
        + Sync
        + 'static,
    W::Error: Send,
{
    Box::pin(async move {
        sync_with_policy(
            context.sources,
            &context.consensus_parameters,
            context.wallet,
            context.sync_mode,
            context.config,
            context.events,
            policy,
        )
        .await
    })
}

macro_rules! strategy {
    ($name:ident, $policy:expr) => {
        impl<P, W> SyncStrategy<P, W> for $name
        where
            P: consensus::Parameters + Sync + Send + 'static,
            W: SyncWallet
                + SyncBlocks
                + SyncTransactions
                + SyncNullifiers
                + SyncOutPoints
                + SyncShardTrees
                + Send
                + Sync
                + 'static,
            W::Error: Send,
        {
            fn run(
                &self,
                context: SyncContext<P, W>,
            ) -> BoxFuture<'static, Result<SyncResult, SyncError<W::Error>>> {
                run(context, $policy)
            }
        }
    };
}

/// Checks the note commitments of a block range against the tree state at its end, unless
/// a source with assumed integrity sent the blocks. Does not run the check if the tree states
/// are from one unverified source.
#[derive(Debug, Clone, Copy, Default)]
pub struct Auto;

/// Does the check of [`Auto`] on every block range. Fails if the tree states are from one
/// unverified source.
#[derive(Debug, Clone, Copy, Default)]
pub struct Verified;

/// Does no extra checks and fails on a result that is not from a source with assumed integrity.
#[derive(Debug, Clone, Copy, Default)]
pub struct AssumedOnly;

strategy!(Auto, Policy::Auto);
strategy!(AssumedOnly, Policy::AssumedOnly);
strategy!(Verified, Policy::Verified);
