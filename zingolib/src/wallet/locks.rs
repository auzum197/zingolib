//! Advisory output locks, as required by `zcash_client_backend`'s `OutputLockStore`.
//!
//! Locks are process-local and are not written to the wallet file. A lock stops an output from
//! being selected for a new transaction until its expiry height passes, its owner releases it,
//! or an owner-scoped policy spends through it.

use std::collections::BTreeMap;

use zcash_client_backend::data_api::locking::{LockFilter, LockOwner, LockedInputPolicy};
use zcash_protocol::consensus::BlockHeight;

use super::output::OutputRef;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OutputLock {
    pub(crate) owner: LockOwner,
    pub(crate) expiry_height: BlockHeight,
}

/// How a lock filter ranks an output during note selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LockTier {
    /// Selected before any deferred output.
    Preferred,
    /// Selected only when the preferred outputs cannot cover the target value.
    Deferred,
    /// Never selected.
    Ineligible,
}

#[derive(Debug, Default)]
pub(crate) struct OutputLocks(BTreeMap<OutputRef, OutputLock>);

impl OutputLocks {
    /// The owner holding `output` locked for a transaction mined at `target_height`.
    pub(crate) fn active_owner(
        &self,
        output: &OutputRef,
        target_height: BlockHeight,
    ) -> Option<LockOwner> {
        self.0
            .get(output)
            .filter(|lock| lock.expiry_height >= target_height)
            .map(|lock| lock.owner)
    }

    pub(crate) fn tier(
        &self,
        output: &OutputRef,
        target_height: BlockHeight,
        lock_filter: LockFilter<'_>,
    ) -> LockTier {
        let LockFilter::Policy(policy) = lock_filter else {
            return LockTier::Preferred;
        };
        match (self.active_owner(output, target_height), policy) {
            (None, LockedInputPolicy::PreferLocked(_)) => LockTier::Deferred,
            (None, _) => LockTier::Preferred,
            (Some(owner), _) if !policy.overridable_owners().contains(&owner) => {
                LockTier::Ineligible
            }
            (Some(_), LockedInputPolicy::PreferLocked(_)) => LockTier::Preferred,
            (Some(_), _) => LockTier::Deferred,
        }
    }

    pub(crate) fn eligible(
        &self,
        output: &OutputRef,
        target_height: BlockHeight,
        lock_filter: LockFilter<'_>,
    ) -> bool {
        self.tier(output, target_height, lock_filter) != LockTier::Ineligible
    }

    /// Whether `owner` may take or extend the lock on `output` as of `chain_tip`.
    ///
    /// An unknown chain tip never counts a foreign lock as expired.
    pub(crate) fn lockable_by(
        &self,
        output: &OutputRef,
        owner: LockOwner,
        chain_tip: Option<BlockHeight>,
    ) -> bool {
        self.0.get(output).is_none_or(|lock| {
            lock.owner == owner || chain_tip.is_some_and(|tip| lock.expiry_height <= tip)
        })
    }

    pub(crate) fn insert(&mut self, output: OutputRef, lock: OutputLock) {
        self.0.insert(output, lock);
    }

    /// Releases `output` if `owner` holds its lock. Returns whether a lock was released.
    pub(crate) fn release(&mut self, output: &OutputRef, owner: LockOwner) -> bool {
        match self.0.get(output) {
            Some(lock) if lock.owner == owner => {
                self.0.remove(output);
                true
            }
            _ => false,
        }
    }

    /// Releases every lock whose output satisfies `predicate`. Returns the number released.
    pub(crate) fn release_where(&mut self, mut predicate: impl FnMut(&OutputRef) -> bool) -> usize {
        let before = self.0.len();
        self.0.retain(|output, _| !predicate(output));
        before - self.0.len()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&OutputRef, &OutputLock)> {
        self.0.iter()
    }
}
