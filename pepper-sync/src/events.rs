//! Push-based sync event stream.
//!
//! The sync loop is the single writer of progress and the single point at which transactions are
//! decrypted. It announces the events it commits so observers subscribe instead of polling
//! [`crate::sync_status`] under the wallet lock.
//!
//! Delivery is best-effort and lag-tolerant. [`tokio::sync::broadcast`] never blocks the sender:
//! a slow or suspended receiver accrues [`tokio::sync::broadcast::error::RecvError::Lagged`] and
//! is fast-forwarded. `Lagged` does not indicate a stall. It signals the consumer to reconcile
//! against persisted wallet state, taking coverage from [`crate::sync_status`] and discoveries
//! from [`crate::wallet::traits::SyncTransactions::get_wallet_transactions`].
//!
//! The engine emits events describing the scan it performed. Derived metrics, including any
//! progress percentage, are the consumer's responsibility: accumulate
//! [`SyncEvent::RangeScanned`] output counts against the totals carried by
//! [`SyncEvent::SessionStarted`].

use std::ops::Range;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use tokio::sync::broadcast;
use zcash_primitives::transaction::TxId;
use zcash_protocol::consensus::BlockHeight;
use zingo_status::confirmation_status::ConfirmationStatus;

use crate::sync::ScanPriority;

/// A committed sync event with its position in the stream.
#[derive(Clone, Debug)]
pub struct SequencedSyncEvent {
    /// Position in the event stream, scoped to the emitter's lifetime. A consumer can use it
    /// to tell whether its live view is current. It carries no chain meaning and cannot be
    /// used to resume the stream after a restart.
    pub seq: u64,
    /// The committed event.
    pub event: SyncEvent,
}

/// Events announced by the sync engine as they are committed to the wallet.
///
/// Events arrive in the order the engine commits them, which differs from chain order under
/// non-linear scanning. Consumers should render the discovered set sorted by height and gate
/// any balance display on contiguous coverage rather than on `seq`.
#[derive(Clone, Debug)]
pub enum SyncEvent {
    /// Emitted once after pre-scan initialisation. Carries the note-commitment totals and the
    /// already-scanned baseline from which a consumer computes its own progress metric.
    SessionStarted {
        /// One block above the fully scanned wallet height at the start of the session.
        sync_start_height: BlockHeight,
        /// Wallet birthday.
        birthday: BlockHeight,
        /// Chain height at the start of the session.
        tip: BlockHeight,
        /// Total sapling note commitments in the birthday..=tip window.
        total_sapling_outputs: u32,
        /// Total orchard note commitments in the birthday..=tip window.
        total_orchard_outputs: u32,
        /// Sapling outputs scanned in previous sessions.
        already_scanned_sapling_outputs: u32,
        /// Orchard outputs scanned in previous sessions.
        already_scanned_orchard_outputs: u32,
        /// Blocks scanned in previous sessions.
        already_scanned_blocks: u32,
    },
    /// A contiguous range was fully scanned and committed. Output counts are tree-size deltas
    /// summed over the batch's scanned blocks.
    RangeScanned {
        /// The committed block range.
        range: Range<BlockHeight>,
        /// The priority the range was scanned with.
        priority: ScanPriority,
        /// Sapling note commitments scanned in this range.
        sapling_outputs: u32,
        /// Orchard note commitments scanned in this range.
        orchard_outputs: u32,
    },
    /// A relevant transaction was decrypted and committed.
    /// Hint only: full data is queryable from the wallet by `txid`.
    TxDiscovered {
        /// Transaction ID.
        txid: TxId,
        /// Confirmation status at commit.
        status: ConfirmationStatus,
    },
    /// Wallet data above `reverted_to` was truncated by a reorg. Subscribers must roll back
    /// state derived from events above this height.
    Reorg {
        /// Highest block height that survived the truncation.
        reverted_to: BlockHeight,
    },
    /// The known chain tip advanced. Monotonic across all sources.
    TipMoved {
        /// The new chain tip.
        to: BlockHeight,
    },
}

/// Sequenced, non-blocking sender of [`SequencedSyncEvent`]s.
///
/// Created by the consumer via [`SyncEmitter::new`] and passed to [`crate::sync()`]. Clones
/// share the same channel and sequence counter.
#[derive(Clone)]
pub struct SyncEmitter {
    seq: Arc<AtomicU64>,
    tx: broadcast::Sender<SequencedSyncEvent>,
}

impl SyncEmitter {
    /// Creates an emitter with the given channel capacity, returning it with the root receiver.
    ///
    /// Capacity governs cadence tolerance: how far a subscriber may fall behind before it lags
    /// and must reconcile. See [`crate::config::SyncConfig::event_channel_capacity`].
    #[must_use]
    pub fn new(capacity: usize) -> (Self, broadcast::Receiver<SequencedSyncEvent>) {
        let (tx, rx) = broadcast::channel(capacity);
        (
            Self {
                seq: Arc::new(AtomicU64::new(0)),
                tx,
            },
            rx,
        )
    }

    /// Creates a new receiver for the live event stream.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<SequencedSyncEvent> {
        self.tx.subscribe()
    }

    /// Non-blocking. `send` errors only when there are no live receivers, in which case the
    /// event is dropped.
    pub(crate) fn emit(&self, event: SyncEvent) {
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let _no_receivers = self.tx.send(SequencedSyncEvent { seq, event });
    }

    pub(crate) fn emit_all(&self, events: impl IntoIterator<Item = SyncEvent>) {
        for event in events {
            self.emit(event);
        }
    }
}

/// Emits [`SyncEvent::TipMoved`] only on a monotonic increase, deduped across the sync loop and
/// the mempool monitor via a shared cell.
pub(crate) fn emit_tip_if_advanced(emitter: &SyncEmitter, cell: &AtomicU64, observed: BlockHeight) {
    let observed_raw = u64::from(u32::from(observed));
    let mut current = cell.load(Ordering::Acquire);
    while observed_raw > current {
        match cell.compare_exchange_weak(current, observed_raw, Ordering::AcqRel, Ordering::Acquire)
        {
            Ok(_) => {
                emitter.emit(SyncEvent::TipMoved { to: observed });
                return;
            }
            Err(now) => current = now,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn height(h: u32) -> BlockHeight {
        BlockHeight::from_u32(h)
    }

    #[test]
    fn seq_strictly_increasing() {
        let (emitter, mut rx) = SyncEmitter::new(16);
        for h in 1..=5 {
            emitter.emit(SyncEvent::TipMoved { to: height(h) });
        }
        let mut last_seq = None;
        while let Ok(event) = rx.try_recv() {
            if let Some(last) = last_seq {
                assert!(event.seq > last);
            }
            last_seq = Some(event.seq);
        }
        assert_eq!(last_seq, Some(4));
    }

    #[test]
    fn emit_without_receivers_is_dropped() {
        let (emitter, rx) = SyncEmitter::new(4);
        drop(rx);
        emitter.emit(SyncEvent::TipMoved { to: height(1) });
    }

    #[test]
    fn tip_monotonic_and_deduped() {
        let (emitter, mut rx) = SyncEmitter::new(16);
        let cell = AtomicU64::new(0);
        emit_tip_if_advanced(&emitter, &cell, height(100));
        emit_tip_if_advanced(&emitter, &cell, height(100));
        emit_tip_if_advanced(&emitter, &cell, height(99));
        emit_tip_if_advanced(&emitter, &cell, height(101));

        let mut tips = Vec::new();
        while let Ok(event) = rx.try_recv() {
            match event.event {
                SyncEvent::TipMoved { to } => tips.push(u32::from(to)),
                event => panic!("unexpected event: {event:?}"),
            }
        }
        assert_eq!(tips, vec![100, 101]);
    }

    #[test]
    fn tip_monotonic_under_concurrent_emitters() {
        let (emitter, mut rx) = SyncEmitter::new(1024);
        let cell = Arc::new(AtomicU64::new(0));
        std::thread::scope(|scope| {
            for offset in 0..4 {
                let emitter = emitter.clone();
                let cell = cell.clone();
                scope.spawn(move || {
                    for h in 1..=100u32 {
                        emit_tip_if_advanced(&emitter, &cell, height(h + offset));
                    }
                });
            }
        });

        let mut last_tip = 0;
        while let Ok(event) = rx.try_recv() {
            if let SyncEvent::TipMoved { to } = event.event {
                assert!(u32::from(to) > last_tip, "tip regressed");
                last_tip = u32::from(to);
            }
        }
        assert_eq!(last_tip, 103);
    }

    #[test]
    fn lagged_subscriber_fast_forwards() {
        let (emitter, mut rx) = SyncEmitter::new(2);
        for h in 1..=5 {
            emitter.emit(SyncEvent::TipMoved { to: height(h) });
        }
        match rx.try_recv() {
            Err(broadcast::error::TryRecvError::Lagged(n)) => assert_eq!(n, 3),
            other => panic!("expected lag, got: {other:?}"),
        }
        // fast-forwarded to the oldest retained event
        let event = rx.try_recv().expect("retained event");
        assert_eq!(event.seq, 3);
    }
}
