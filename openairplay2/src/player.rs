//! The playback queue — a dedicated OS thread that feeds decoded PCM to the
//! host's [`AudioSink`].
//!
//! Timing is deliberately "soft" (no PTP): the sender buffers audio ahead and
//! we drain it at the sink's rate. Blocking `AudioSink::write` calls pace this
//! thread, and the TCP reader backpressures on the queued-sample count so
//! latency and memory stay bounded.
//!
//! Transport control is **out-of-band** from the audio queue (an in-band
//! command would sit behind the ~2 s buffer and only act seconds later):
//!
//! - **Pause holds; it never drops.** While the persistent `paused` flag is
//!   set, arriving audio is parked in a hold buffer and the queued-sample
//!   count keeps growing, so the TCP reader backpressures and the sender's
//!   send cursor freezes near the pause point. This is load-bearing for
//!   correctness, not just latency: a sender may pause with a bare `rate=0`
//!   (no flush), and it then expects every frame it already sent to still be
//!   buffered when it resumes — resume plays the held audio immediately.
//! - **Flush discards exactly what the sender named.** `FLUSHBUFFERED`
//!   carries a `flushUntilSeq`; each queued packet is stamped with its packet
//!   sequence number, and a flush discards only stamps below the boundary —
//!   from the queue and the hold buffer alike — retaining everything at or
//!   after it. (A flush without a boundary discards everything queued.)
//!
//! Pause and flush also call [`AudioSink::flush`] so the sink discards
//! whatever it has buffered of its own — audio already handed over must go
//! silent now, not after the hardware buffer drains.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::Arc;
use std::thread::JoinHandle;

use log::debug;

use crate::sink::AudioSink;

/// Queue high-water mark (interleaved samples): above this the reader
/// backpressures so latency/memory stay bounded. ~2 s of audio.
pub fn max_queued_samples(rate: u32, channels: u8) -> usize {
    rate as usize * channels as usize * 2
}

/// `flush_request` encoding: 0 = none, [`FLUSH_ALL`] = discard everything
/// queued, otherwise `boundary + 1` (discard stamps below `boundary`).
const FLUSH_NONE: u64 = 0;
const FLUSH_ALL: u64 = u64::MAX;

enum Command {
    /// (packet sequence number, interleaved samples).
    Pcm(u64, Vec<i16>),
    /// Nudge the loop to re-check the paused flag / flush request (used when
    /// the queue is idle so control is still noticed promptly).
    Wake,
    Stop,
}

/// A cloneable handle for feeding decoded PCM to the playback thread and
/// controlling transport (pause/resume, flush).
#[derive(Clone)]
pub struct PlayerSender {
    tx: Sender<Command>,
    /// Interleaved samples sent but not yet played or discarded. Held audio
    /// stays counted — that is what keeps backpressure engaged across a pause.
    pending: Arc<AtomicUsize>,
    /// Latest unconsumed flush request (see `FLUSH_NONE`/`FLUSH_ALL`).
    flush_request: Arc<AtomicU64>,
    /// While true the queue holds all audio (delivering silence).
    paused: Arc<AtomicBool>,
}

impl PlayerSender {
    /// Queue a decoded packet, stamped with its packet sequence number.
    pub fn play(&self, seq: u64, pcm: Vec<i16>) {
        self.pending.fetch_add(pcm.len(), Ordering::Relaxed);
        let _ = self.tx.send(Command::Pcm(seq, pcm));
    }

    /// Engage/release the pause gate. While paused the queue holds all audio;
    /// releasing it plays the held audio from where playback stopped.
    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
        let _ = self.tx.send(Command::Wake);
    }

    /// Discard buffered audio with a sequence stamp below `below_seq`
    /// (seek/skip: the `FLUSHBUFFERED` boundary), retaining the rest. `None`
    /// discards everything currently buffered.
    pub fn flush(&self, below_seq: Option<u64>) {
        let encoded = below_seq.map_or(FLUSH_ALL, |seq| seq.saturating_add(1));
        self.flush_request.store(encoded, Ordering::Relaxed);
        let _ = self.tx.send(Command::Wake);
    }

    /// Interleaved samples currently queued or held — the backpressure signal.
    pub fn pending_samples(&self) -> usize {
        self.pending.load(Ordering::Relaxed)
    }

    /// Whether the pause gate is engaged (test inspection only).
    #[cfg(test)]
    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }
}

/// Owns the playback thread; dropping it stops the thread and drops the sink.
pub struct Player {
    tx: Option<Sender<Command>>,
    handle: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    pending: Arc<AtomicUsize>,
    flush_request: Arc<AtomicU64>,
    paused: Arc<AtomicBool>,
}

impl Player {
    /// Spawn the playback thread feeding `sink`.
    pub fn spawn(sink: Box<dyn AudioSink>) -> Player {
        let (tx, rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let pending = Arc::new(AtomicUsize::new(0));
        let flush_request = Arc::new(AtomicU64::new(FLUSH_NONE));
        let paused = Arc::new(AtomicBool::new(false));
        let ctx = RunCtx {
            stop: stop.clone(),
            pending: pending.clone(),
            flush_request: flush_request.clone(),
            paused: paused.clone(),
        };
        let handle = std::thread::Builder::new()
            .name("audio-player".into())
            .spawn(move || run(sink, rx, ctx))
            .expect("spawn player thread");
        Player {
            tx: Some(tx),
            handle: Some(handle),
            stop,
            pending,
            flush_request,
            paused,
        }
    }

    pub fn sender(&self) -> PlayerSender {
        PlayerSender {
            tx: self.tx.clone().expect("sender available before drop"),
            pending: self.pending.clone(),
            flush_request: self.flush_request.clone(),
            paused: self.paused.clone(),
        }
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(tx) = &self.tx {
            let _ = tx.send(Command::Stop);
        }
        self.tx = None;
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Shared state the playback thread watches for out-of-band control.
struct RunCtx {
    stop: Arc<AtomicBool>,
    pending: Arc<AtomicUsize>,
    flush_request: Arc<AtomicU64>,
    paused: Arc<AtomicBool>,
}

fn run(mut sink: Box<dyn AudioSink>, rx: Receiver<Command>, ctx: RunCtx) {
    // Audio taken off the channel but not yet played or discarded: everything
    // parked during a pause, and anything retained across a flush. Samples in
    // here are still counted in `pending` — held audio must keep the reader
    // backpressured.
    let mut held: VecDeque<(u64, Vec<i16>)> = VecDeque::new();
    let mut was_paused = false;
    let mut packets: u64 = 0;
    'outer: while let Ok(command) = rx.recv() {
        if ctx.stop.load(Ordering::Relaxed) {
            break;
        }

        // React to out-of-band control before touching audio, so it isn't
        // stuck behind the buffer.
        let paused = ctx.paused.load(Ordering::Relaxed);
        let flush = ctx.flush_request.swap(FLUSH_NONE, Ordering::Relaxed);
        let just_paused = paused && !was_paused;
        if flush != FLUSH_NONE || just_paused {
            // The sink discards its own buffers → immediate silence.
            sink.flush();
        }
        if just_paused {
            debug!("player: paused");
        } else if !paused && was_paused {
            debug!("player: resumed");
        }
        was_paused = paused;

        match command {
            Command::Stop => break,
            Command::Wake => {}
            Command::Pcm(seq, pcm) => held.push_back((seq, pcm)),
        }

        if flush != FLUSH_NONE {
            // Move everything queued into `held`, then discard exactly what
            // the flush names: stamps below the boundary (or all of it).
            loop {
                match rx.try_recv() {
                    Ok(Command::Pcm(seq, pcm)) => held.push_back((seq, pcm)),
                    Ok(Command::Wake) => {}
                    Ok(Command::Stop) => break 'outer,
                    Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
                }
            }
            let before = held.len();
            held.retain(|(seq, pcm)| {
                let keep = flush != FLUSH_ALL && seq.saturating_add(1) >= flush;
                if !keep {
                    ctx.pending.fetch_sub(pcm.len(), Ordering::Relaxed);
                }
                keep
            });
            debug!(
                "player: flushed {} packets, retained {}",
                before - held.len(),
                held.len()
            );
        }

        if paused {
            continue; // hold everything; backpressure freezes the sender
        }

        // Deliver held audio in order; blocking writes pace playback. Break
        // out between writes if out-of-band control arrives.
        while let Some((_, pcm)) = held.pop_front() {
            ctx.pending.fetch_sub(pcm.len(), Ordering::Relaxed);
            packets += 1;
            if packets <= 3 || packets.is_multiple_of(250) {
                debug!("player: {packets} packets");
            }
            sink.write(&pcm);
            if ctx.stop.load(Ordering::Relaxed) {
                break 'outer;
            }
            if ctx.paused.load(Ordering::Relaxed)
                || ctx.flush_request.load(Ordering::Relaxed) != FLUSH_NONE
            {
                // Handled at the top of the loop; the control call also sent
                // a Wake, so recv() returns promptly.
                break;
            }
        }
    }
    debug!("player: stopped, {packets} packets");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Mutex;
    use std::time::Duration;

    /// Records every delivered packet and every flush.
    #[derive(Clone, Default)]
    struct Recorder {
        writes: Arc<Mutex<Vec<Vec<i16>>>>,
        flushes: Arc<AtomicUsize>,
    }

    impl AudioSink for Recorder {
        fn write(&mut self, pcm: &[i16]) {
            self.writes.lock().unwrap().push(pcm.to_vec());
        }
        fn flush(&mut self) {
            self.flushes.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// A recorder whose `write` signals entry and then blocks until released,
    /// so tests can hold the playback thread mid-write deterministically.
    struct GatedRecorder {
        recorder: Recorder,
        entered: std::sync::mpsc::Sender<()>,
        release: std::sync::mpsc::Receiver<()>,
    }

    impl AudioSink for GatedRecorder {
        fn write(&mut self, pcm: &[i16]) {
            let _ = self.entered.send(());
            let _ = self.release.recv();
            self.recorder.write(pcm);
        }
        fn flush(&mut self) {
            self.recorder.flush();
        }
    }

    /// Wait (bounded) until the queue is drained.
    fn settle(sender: &PlayerSender) {
        for _ in 0..400 {
            if sender.pending_samples() == 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(sender.pending_samples(), 0);
    }

    /// Wait (bounded) until `pending` reaches `expected` and assert it stays
    /// there — for asserting audio is *held*, not dropped.
    fn assert_pending_holds_at(sender: &PlayerSender, expected: usize) {
        for _ in 0..400 {
            if sender.pending_samples() == expected {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(sender.pending_samples(), expected);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            sender.pending_samples(),
            expected,
            "held audio must stay counted"
        );
    }

    #[test]
    fn max_queued_for_44100_stereo() {
        assert_eq!(max_queued_samples(44100, 2), 176_400); // 2 s interleaved
    }

    #[test]
    fn delivers_packets_in_order() {
        let recorder = Recorder::default();
        let writes = recorder.writes.clone();
        let player = Player::spawn(Box::new(recorder));
        let sender = player.sender();
        sender.play(1, vec![1i16; 4]);
        sender.play(2, vec![2i16; 4]);
        sender.play(3, vec![3i16; 4]);
        settle(&sender);
        drop(player);
        assert_eq!(
            *writes.lock().unwrap(),
            vec![vec![1i16; 4], vec![2i16; 4], vec![3i16; 4]]
        );
    }

    #[test]
    fn pause_holds_delivery_then_resume_plays_held_audio() {
        let recorder = Recorder::default();
        let writes = recorder.writes.clone();
        let flushes = recorder.flushes.clone();
        let player = Player::spawn(Box::new(recorder));
        let sender = player.sender();

        sender.set_paused(true);
        assert!(sender.is_paused());
        sender.play(1, vec![1i16; 4]);
        sender.play(2, vec![2i16; 4]);

        // The audio is held, not dropped: pending stays up (the backpressure
        // signal) and nothing reaches the sink.
        assert_pending_holds_at(&sender, 8);
        assert!(
            writes.lock().unwrap().is_empty(),
            "paused audio must not play"
        );
        assert!(
            flushes.load(Ordering::Relaxed) >= 1,
            "pause must flush the sink for immediate silence"
        );

        // Resume: the held audio plays from where playback stopped, then new
        // audio continues.
        sender.set_paused(false);
        sender.play(3, vec![3i16; 4]);
        settle(&sender);
        drop(player);
        assert_eq!(
            *writes.lock().unwrap(),
            vec![vec![1i16; 4], vec![2i16; 4], vec![3i16; 4]]
        );
    }

    #[test]
    fn flush_discards_below_boundary_and_retains_the_rest() {
        let recorder = Recorder::default();
        let writes = recorder.writes.clone();
        let flushes = recorder.flushes.clone();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let player = Player::spawn(Box::new(GatedRecorder {
            recorder,
            entered: entered_tx,
            release: release_rx,
        }));
        let sender = player.sender();

        // Hold the thread inside write(seq 10) while 11/12/13 queue behind it.
        sender.play(10, vec![1i16; 4]);
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("thread entered write");
        sender.play(11, vec![2i16; 4]);
        sender.play(12, vec![3i16; 4]);
        sender.play(13, vec![4i16; 4]);

        // Seek: discard exactly seq < 13, retain seq 13.
        sender.flush(Some(13));
        release_tx.send(()).unwrap();
        // Only the retained packet still needs a release.
        release_tx.send(()).unwrap();

        settle(&sender);
        drop(player);
        assert_eq!(
            *writes.lock().unwrap(),
            vec![vec![1i16; 4], vec![4i16; 4]],
            "pre-flush write plays; below-boundary audio is discarded; \
             at/after-boundary audio is retained"
        );
        assert_eq!(flushes.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn flush_without_boundary_discards_everything_queued() {
        let recorder = Recorder::default();
        let writes = recorder.writes.clone();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let player = Player::spawn(Box::new(GatedRecorder {
            recorder,
            entered: entered_tx,
            release: release_rx,
        }));
        let sender = player.sender();

        sender.play(1, vec![1i16; 4]);
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("thread entered write");
        sender.play(2, vec![2i16; 4]);
        sender.play(3, vec![3i16; 4]);

        sender.flush(None);
        release_tx.send(()).unwrap();

        settle(&sender);
        drop(player);
        assert_eq!(*writes.lock().unwrap(), vec![vec![1i16; 4]]);
    }

    #[test]
    fn flush_during_pause_discards_held_below_boundary_only() {
        let recorder = Recorder::default();
        let writes = recorder.writes.clone();
        let player = Player::spawn(Box::new(recorder));
        let sender = player.sender();

        sender.set_paused(true);
        sender.play(5, vec![5i16; 4]);
        sender.play(6, vec![6i16; 4]);
        sender.play(7, vec![7i16; 4]);
        assert_pending_holds_at(&sender, 12);

        // A pause-with-flush names a boundary; held audio below it goes,
        // held audio at/after it survives the pause.
        sender.flush(Some(7));
        assert_pending_holds_at(&sender, 4);

        sender.set_paused(false);
        settle(&sender);
        drop(player);
        assert_eq!(*writes.lock().unwrap(), vec![vec![7i16; 4]]);
    }

    #[test]
    fn backpressure_counter_rises_and_falls() {
        let recorder = Recorder::default();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let player = Player::spawn(Box::new(GatedRecorder {
            recorder,
            entered: entered_tx,
            release: release_rx,
        }));
        let sender = player.sender();

        sender.play(1, vec![0i16; 100]);
        entered_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("thread entered write");
        // The first packet was taken and decremented before its write; the
        // rest are pending while the sink blocks.
        sender.play(2, vec![0i16; 100]);
        sender.play(3, vec![0i16; 100]);
        assert_eq!(sender.pending_samples(), 200);

        // Release all writes; the queue drains back to zero.
        for _ in 0..3 {
            let _ = release_tx.send(());
        }
        settle(&sender);
        drop(player);
    }
}
