//! The active-session slot: AirPlay 2 is **last stream wins**.
//!
//! A second sender that starts playing interrupts whoever is playing and
//! takes over — there is no "busy" refusal in AirPlay 2. Verified against a
//! HomePod (the interrupted sender's audio stops, its player goes to paused,
//! and it disconnects from the receiver), and shairport-sync hardcodes the
//! same for AirPlay 2: its play lock is acquired at the initial SETUP with
//! interruption always allowed, terminating the previous connection. See
//! plans/20260808-04-sender-takeover.md.
//!
//! Two things have to be true for a handover to be clean, and this module
//! provides both:
//!
//! - The interrupted connection is told to **close**, because that is the
//!   entire signal a sender needs — it pauses itself and drops the route.
//! - The new stream does not start until the old one has **finished tearing
//!   down**, so the two never hold the host's audio device at once (an
//!   exclusive ALSA device would otherwise refuse the second one).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use log::{debug, warn};
use tokio::sync::{oneshot, Notify};

/// How long a takeover waits for the interrupted connection to finish
/// tearing down (its playback thread joined, the host's sink dropped)
/// before proceeding anyway. Generous for a queue flush and a device close;
/// short enough that a wedged session cannot hold the new sender hostage —
/// which by specification wins regardless.
const TAKEOVER_TIMEOUT: Duration = Duration::from_secs(2);

/// Connection identity, only ever compared for equality: it distinguishes
/// "this connection is re-SETUPing" from "a different sender is taking
/// over".
pub fn next_connection_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// The one slot a playing connection occupies. Shared by every connection of
/// one receiver.
#[derive(Default)]
pub struct ActiveSlot {
    holder: Mutex<Option<Holder>>,
}

struct Holder {
    id: u64,
    /// Notified when someone else claims the slot; the holder's request loop
    /// wakes on it and closes the connection.
    evict: Arc<Notify>,
    /// Resolves when the holder has finished tearing down. Nothing is ever
    /// sent: the holder owns the sender inside its session and the *drop* is
    /// the signal, so a panicking or aborted connection also releases it.
    finished: oneshot::Receiver<()>,
}

impl ActiveSlot {
    /// Claim the slot for connection `id`, interrupting whoever holds it.
    ///
    /// Returns `None` when this connection already holds it (a sender may
    /// SETUP more than once on one connection — phase 1 then phase 2 — and
    /// must not interrupt itself). Otherwise the previous holder is told to
    /// close, its teardown is awaited (bounded by [`TAKEOVER_TIMEOUT`]), and
    /// the returned guard must be kept until *this* connection has finished
    /// tearing down — see [`ActiveGuard`].
    pub async fn claim(self: &Arc<Self>, id: u64, evict: Arc<Notify>) -> Option<ActiveGuard> {
        let (finished_tx, finished_rx) = oneshot::channel();
        // Take the previous holder out under the lock, and release the lock
        // before awaiting it.
        let previous = {
            let mut holder = self.holder.lock().unwrap();
            if holder.as_ref().is_some_and(|h| h.id == id) {
                return None;
            }
            holder.replace(Holder {
                id,
                evict,
                finished: finished_rx,
            })
        };
        if let Some(previous) = previous {
            debug!(
                "takeover: connection {id} interrupts connection {}",
                previous.id
            );
            // A stored permit, not a broadcast: the holder may be busy with a
            // request right now and must still see this when it next reads.
            previous.evict.notify_one();
            match tokio::time::timeout(TAKEOVER_TIMEOUT, previous.finished).await {
                Ok(_) => debug!(
                    "takeover: connection {} released the audio device",
                    previous.id
                ),
                Err(_) => warn!(
                    "takeover: connection {} did not finish within {TAKEOVER_TIMEOUT:?}; \
                     starting the new stream anyway",
                    previous.id
                ),
            }
        }
        Some(ActiveGuard {
            slot: self.clone(),
            id,
            _finished: finished_tx,
        })
    }

    /// Whether `id` currently holds the slot (tests, and callers that want to
    /// describe the state).
    pub fn is_held_by(&self, id: u64) -> bool {
        self.holder
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|h| h.id == id)
    }

    /// Whether anything holds the slot.
    pub fn is_held(&self) -> bool {
        self.holder.lock().unwrap().is_some()
    }
}

/// Proof that a connection owns the active-session slot.
///
/// It lives **inside the session**, so that it is dropped only after the
/// session's playback thread has been joined and the host's sink released
/// ([`Session::drop`](crate::session::Session) does this in order). Dropping
/// it is what lets a waiting taker-over start its own stream, and it also
/// vacates the slot if this connection still holds it.
pub struct ActiveGuard {
    slot: Arc<ActiveSlot>,
    id: u64,
    /// Never sent on; see [`Holder::finished`].
    _finished: oneshot::Sender<()>,
}

impl Drop for ActiveGuard {
    fn drop(&mut self) {
        let mut holder = self.slot.holder.lock().unwrap();
        // Only if we are still the holder: after being taken over, the slot
        // belongs to the new connection and must be left alone.
        if holder.as_ref().is_some_and(|h| h.id == self.id) {
            *holder = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_free_slot_is_taken() {
        let slot = Arc::new(ActiveSlot::default());
        let guard = slot.claim(1, Arc::new(Notify::new())).await;
        assert!(guard.is_some(), "a free slot must be claimable");
        assert!(slot.is_held_by(1));

        // Dropping the guard vacates it.
        drop(guard);
        assert!(!slot.is_held());
    }

    #[tokio::test]
    async fn the_holder_does_not_interrupt_itself() {
        let slot = Arc::new(ActiveSlot::default());
        let evict = Arc::new(Notify::new());
        let _guard = slot.claim(7, evict.clone()).await.expect("first claim");

        // A second SETUP on the same connection: no new guard, still held,
        // and no eviction signal left behind for its own request loop.
        assert!(slot.claim(7, evict.clone()).await.is_none());
        assert!(slot.is_held_by(7));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), evict.notified())
                .await
                .is_err(),
            "a re-SETUP must not evict its own connection"
        );
    }

    #[tokio::test]
    async fn another_connection_evicts_the_holder_and_waits_for_it() {
        let slot = Arc::new(ActiveSlot::default());
        let evict = Arc::new(Notify::new());
        let first = slot.claim(1, evict.clone()).await.expect("first claim");

        // The taker-over blocks until the first guard is dropped. It keeps the
        // guard it gets: dropping one vacates the slot.
        let taking = tokio::spawn({
            let slot = slot.clone();
            async move { slot.claim(2, Arc::new(Notify::new())).await }
        });
        // The holder is told to close.
        tokio::time::timeout(Duration::from_secs(1), evict.notified())
            .await
            .expect("the holder must be evicted");
        assert!(
            !taking.is_finished(),
            "the takeover must await the teardown"
        );

        drop(first); // teardown complete: sink released
        let _second = taking
            .await
            .unwrap()
            .expect("the takeover must then succeed");
        assert!(slot.is_held_by(2));
    }

    #[tokio::test(start_paused = true)]
    async fn a_wedged_holder_only_delays_the_takeover() {
        let slot = Arc::new(ActiveSlot::default());
        // A holder that never tears down: its guard is leaked, so the
        // completion signal never comes.
        std::mem::forget(slot.claim(1, Arc::new(Notify::new())).await.expect("claim"));

        // The new sender wins anyway, after the timeout (time is auto-
        // advanced by the paused clock, so this does not actually wait).
        let started = tokio::time::Instant::now();
        let _guard = slot
            .claim(2, Arc::new(Notify::new()))
            .await
            .expect("the new sender must win regardless");
        assert!(started.elapsed() >= TAKEOVER_TIMEOUT);
        assert!(slot.is_held_by(2));
    }
}
