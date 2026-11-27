//! Source selection for each request.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use crate::{Integrity, Request, SourceError, SourceExposure, SourceId, SourceInfo};

/// The sources for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Route {
    /// One source after the other, until a request succeeds.
    Failover(Vec<SourceId>),
    /// All the sources. `need` responses must be equal.
    Quorum {
        /// Sources that receive the request.
        of: Vec<SourceId>,
        /// Number of equal responses that are necessary.
        need: usize,
    },
}

/// Selects the sources for each request.
pub trait Router: Send + Sync {
    /// Selects the route for `request`. A [`SourceId`] is an index into `sources`.
    fn select_route(&self, request: &Request, sources: &[SourceInfo]) -> Route;

    /// A request to `source` succeeded.
    fn on_success(&self, _source: SourceId) {}

    /// A request to `source` failed, or data from `source` failed a check.
    fn on_failure(&self, _source: SourceId, _error: &SourceError) {}
}

/// The state of a source in a [`Failover`] or [`Quorum`] router.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum SourceHealth {
    /// The last request succeeded, or no request was sent.
    Healthy,
    /// This number of requests failed after the last success. The source is used last.
    Failing(u32),
    /// Data from the source failed a check. The source is no longer used.
    Dropped,
}

/// A router in an [`Arc`] is a router, so you can keep one copy and read its state.
impl<R: Router + ?Sized> Router for Arc<R> {
    fn select_route(&self, request: &Request, sources: &[SourceInfo]) -> Route {
        (**self).select_route(request, sources)
    }

    fn on_success(&self, source: SourceId) {
        (**self).on_success(source);
    }

    fn on_failure(&self, source: SourceId, error: &SourceError) {
        (**self).on_failure(source, error);
    }
}

#[derive(Debug, Default)]
struct HealthLog(Mutex<HashMap<SourceId, SourceHealth>>);

impl HealthLog {
    fn on_success(&self, source: SourceId) {
        let mut log = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(SourceHealth::Failing(_)) = log.get(&source) {
            log.remove(&source);
        }
    }

    fn on_failure(&self, source: SourceId, error: &SourceError) {
        let mut log = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let failures = match log.get(&source) {
            Some(SourceHealth::Dropped) => return,
            Some(SourceHealth::Failing(n)) => *n,
            Some(SourceHealth::Healthy) | None => 0,
        };
        match error {
            SourceError::FailedVerification => {
                log.insert(source, SourceHealth::Dropped);
            }
            SourceError::Timeout(_)
            | SourceError::Unavailable(_)
            | SourceError::Interrupted(_)
            | SourceError::InvalidData(_)
            | SourceError::Other(_) => {
                log.insert(source, SourceHealth::Failing(failures.saturating_add(1)));
            }
            _ => {}
        }
    }

    fn health(&self, source: SourceId) -> SourceHealth {
        let log = self.0.lock().unwrap_or_else(|e| e.into_inner());
        log.get(&source).copied().unwrap_or(SourceHealth::Healthy)
    }

    /// Sources that can serve `request`: fewest failures first, then by integrity and privacy.
    fn candidates(&self, request: &Request, sources: &[SourceInfo]) -> Vec<SourceId> {
        let log = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let identifying = request.wallet_identifier().is_some();
        let mut ranked: Vec<_> = sources
            .iter()
            .enumerate()
            .filter(|(_, info)| info.capabilities.supports(request))
            .filter_map(|(index, info)| {
                let id = SourceId(index);
                let failures = match log.get(&id) {
                    Some(SourceHealth::Dropped) => return None,
                    Some(SourceHealth::Failing(n)) => *n,
                    Some(SourceHealth::Healthy) | None => 0,
                };
                let preference = match (
                    info.integrity,
                    identifying,
                    info.transport.exposed_to_source,
                ) {
                    (Integrity::Assumed, ..) => 0,
                    (Integrity::Unverified, true, SourceExposure::Nothing) => 1,
                    (Integrity::Unverified, true, SourceExposure::Pseudonym) => 2,
                    (Integrity::Unverified, ..) => 3,
                };
                Some((failures, preference, id))
            })
            .collect();
        ranked.sort();
        ranked.into_iter().map(|(_, _, id)| id).collect()
    }
}

/// One source for each request. Sources with assumed integrity are first.
#[derive(Debug, Default)]
pub struct Failover(HealthLog);

impl Failover {
    /// The state of `source`.
    pub fn health(&self, source: SourceId) -> SourceHealth {
        self.0.health(source)
    }
}

impl Router for Failover {
    fn select_route(&self, request: &Request, sources: &[SourceInfo]) -> Route {
        Route::Failover(self.0.candidates(request, sources))
    }

    fn on_success(&self, source: SourceId) {
        self.0.on_success(source);
    }

    fn on_failure(&self, source: SourceId, error: &SourceError) {
        self.0.on_failure(source, error);
    }
}

/// Small responses need a quorum, unless a source with assumed integrity is available.
/// Block ranges and transactions use one source.
#[derive(Debug)]
pub struct Quorum {
    need: usize,
    health: HealthLog,
}

impl Quorum {
    /// `need` responses must be equal.
    pub fn new(need: usize) -> Self {
        Self {
            need,
            health: HealthLog::default(),
        }
    }

    /// The state of `source`.
    pub fn health(&self, source: SourceId) -> SourceHealth {
        self.health.health(source)
    }
}

impl Router for Quorum {
    fn select_route(&self, request: &Request, sources: &[SourceInfo]) -> Route {
        let candidates = self.health.candidates(request, sources);
        let comparable = matches!(
            request,
            Request::Tip | Request::Block(_) | Request::TreeState(_) | Request::SubtreeRoots { .. }
        );
        let assumed_first = candidates
            .first()
            .is_some_and(|id| sources[id.0].integrity == Integrity::Assumed);
        if comparable && !assumed_first {
            Route::Quorum {
                of: candidates,
                need: self.need,
            }
        } else {
            Route::Failover(candidates)
        }
    }

    fn on_success(&self, source: SourceId) {
        self.health.on_success(source);
    }

    fn on_failure(&self, source: SourceId, error: &SourceError) {
        self.health.on_failure(source, error);
    }
}
