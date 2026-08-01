//! The playback queue — a dedicated OS thread that feeds decoded PCM to the
//! host's [`AudioSink`].
//!
//! Timing is deliberately "soft" (no PTP): the Mac buffers audio ahead and we
//! drain it at the sink's rate. Blocking `AudioSink::write` calls pace this
//! thread, and the TCP reader backpressures on the queued-sample count so
//! latency and memory stay bounded.
//!
//! Transport control is **out-of-band** from the audio queue (an in-band
//! command would sit behind the ~2 s buffer and only act seconds later):
//!
//! - **Pause** is a persistent `paused` flag. While set, the queue drops all
//!   audio — the Mac keeps sending buffered-ahead audio, so a one-shot flush
//!   wouldn't stop it; the gate must stay engaged until resume.
//! - **Flush** (seek) bumps a generation counter; each queued packet is stamped
//!   with its generation, and stale-stamped packets are dropped. New audio
//!   for the new position (a fresh generation) plays.
//!
//! Both also call [`AudioSink::flush`] so the sink discards whatever it has
//! buffered of its own — the audio already handed over must go silent now,
//! not after the hardware buffer drains.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;

use log::debug;

use crate::sink::AudioSink;

/// Queue high-water mark (interleaved samples): above this the reader
/// backpressures so latency/memory stay bounded. ~2 s of audio.
pub fn max_queued_samples(rate: u32, channels: u8) -> usize {
    rate as usize * channels as usize * 2
}

enum Command {
    /// (flush generation the packet was produced under, interleaved samples).
    Pcm(u64, Vec<i16>),
    /// Nudge the loop to re-check the paused flag / flush generation (used when
    /// the queue is idle so control is still noticed promptly).
    Wake,
    Stop,
}

/// A cloneable handle for feeding decoded PCM to the playback thread and
/// controlling transport (pause/resume, flush).
#[derive(Clone)]
pub struct PlayerSender {
    tx: Sender<Command>,
    /// Interleaved samples sent but not yet taken by the playback thread.
    pending: Arc<AtomicUsize>,
    /// Bumped on every flush; stamps outgoing audio so stale audio is dropped.
    flush_gen: Arc<AtomicU64>,
    /// While true the queue drops all audio and holds silence.
    paused: Arc<AtomicBool>,
}

impl PlayerSender {
    pub fn play(&self, pcm: Vec<i16>) {
        let gen = self.flush_gen.load(Ordering::Relaxed);
        self.pending.fetch_add(pcm.len(), Ordering::Relaxed);
        let _ = self.tx.send(Command::Pcm(gen, pcm));
    }

    /// Engage/release the pause gate. While paused the queue drops all audio.
    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
        let _ = self.tx.send(Command::Wake);
    }

    /// Drop currently-buffered audio (seek/skip). Bumps the generation so
    /// already-queued audio is discarded; new audio still plays.
    pub fn flush(&self) {
        self.flush_gen.fetch_add(1, Ordering::Relaxed);
        let _ = self.tx.send(Command::Wake);
    }

    /// Interleaved samples currently queued — the backpressure signal.
    pub fn pending_samples(&self) -> usize {
        self.pending.load(Ordering::Relaxed)
    }

    /// Current flush generation (test inspection only).
    #[cfg(test)]
    pub fn generation(&self) -> u64 {
        self.flush_gen.load(Ordering::Relaxed)
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
    flush_gen: Arc<AtomicU64>,
    paused: Arc<AtomicBool>,
}

impl Player {
    /// Spawn the playback thread feeding `sink`.
    pub fn spawn(sink: Box<dyn AudioSink>) -> Player {
        let (tx, rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let pending = Arc::new(AtomicUsize::new(0));
        let flush_gen = Arc::new(AtomicU64::new(0));
        let paused = Arc::new(AtomicBool::new(false));
        let ctx = RunCtx {
            stop: stop.clone(),
            pending: pending.clone(),
            flush_gen: flush_gen.clone(),
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
            flush_gen,
            paused,
        }
    }

    pub fn sender(&self) -> PlayerSender {
        PlayerSender {
            tx: self.tx.clone().expect("sender available before drop"),
            pending: self.pending.clone(),
            flush_gen: self.flush_gen.clone(),
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
    flush_gen: Arc<AtomicU64>,
    paused: Arc<AtomicBool>,
}

fn run(mut sink: Box<dyn AudioSink>, rx: Receiver<Command>, ctx: RunCtx) {
    let mut cur_gen: u64 = 0;
    let mut was_paused = false;
    let mut packets: u64 = 0;
    'outer: while let Ok(command) = rx.recv() {
        if ctx.stop.load(Ordering::Relaxed) {
            break;
        }

        // React to out-of-band control before touching audio, so it isn't
        // stuck behind the buffer.
        let gen = ctx.flush_gen.load(Ordering::Relaxed);
        let paused = ctx.paused.load(Ordering::Relaxed);
        let flushed = gen != cur_gen;
        let just_paused = paused && !was_paused;
        if flushed || just_paused {
            cur_gen = gen;
            // The sink discards its own buffers → immediate silence.
            sink.flush();
            // Drop everything already queued (stale on flush; unwanted on pause).
            loop {
                match rx.try_recv() {
                    Ok(Command::Pcm(_, p)) => {
                        ctx.pending.fetch_sub(p.len(), Ordering::Relaxed);
                    }
                    Ok(Command::Wake) => {}
                    Ok(Command::Stop) => break 'outer,
                    Err(_) => break,
                }
            }
            if flushed {
                debug!("player: flushed (gen {gen})");
            }
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
            Command::Pcm(stamp, pcm) => {
                ctx.pending.fetch_sub(pcm.len(), Ordering::Relaxed);
                if paused || stamp != cur_gen {
                    continue; // paused, or produced before a flush → drop
                }
                packets += 1;
                if packets <= 3 || packets.is_multiple_of(250) {
                    debug!("player: {packets} packets");
                }
                sink.write(&pcm);
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
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(sender.pending_samples(), 0);
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
        sender.play(vec![1i16; 4]);
        sender.play(vec![2i16; 4]);
        sender.play(vec![3i16; 4]);
        settle(&sender);
        drop(player);
        assert_eq!(
            *writes.lock().unwrap(),
            vec![vec![1i16; 4], vec![2i16; 4], vec![3i16; 4]]
        );
    }

    #[test]
    fn pause_drops_delivery_and_flushes_sink() {
        let recorder = Recorder::default();
        let writes = recorder.writes.clone();
        let flushes = recorder.flushes.clone();
        let player = Player::spawn(Box::new(recorder));
        let sender = player.sender();

        sender.set_paused(true);
        assert!(sender.is_paused());
        sender.play(vec![1i16; 4]);
        sender.play(vec![2i16; 4]);
        settle(&sender);
        assert!(
            writes.lock().unwrap().is_empty(),
            "paused audio must not play"
        );
        assert!(
            flushes.load(Ordering::Relaxed) >= 1,
            "pause must flush the sink for immediate silence"
        );

        // Resume: new audio plays again.
        sender.set_paused(false);
        sender.play(vec![3i16; 4]);
        settle(&sender);
        drop(player);
        assert_eq!(*writes.lock().unwrap(), vec![vec![3i16; 4]]);
    }

    #[test]
    fn flush_drops_queued_pcm_and_flushes_sink() {
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

        // Hold the thread inside write(p1) while p2/p3 queue up behind it.
        sender.play(vec![1i16; 4]);
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("thread entered write");
        sender.play(vec![2i16; 4]);
        sender.play(vec![3i16; 4]);

        // Seek: queued (stale-generation) audio must never reach the sink.
        sender.flush();
        assert_eq!(sender.generation(), 1);
        release_tx.send(()).unwrap();

        settle(&sender);
        drop(player);
        assert_eq!(*writes.lock().unwrap(), vec![vec![1i16; 4]]);
        assert_eq!(flushes.load(Ordering::Relaxed), 1);
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

        sender.play(vec![0i16; 100]);
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("thread entered write");
        // The first packet was taken off the queue; the rest are pending
        // while the sink blocks.
        sender.play(vec![0i16; 100]);
        sender.play(vec![0i16; 100]);
        assert_eq!(sender.pending_samples(), 200);

        // Release all writes; the queue drains back to zero.
        for _ in 0..3 {
            let _ = release_tx.send(());
        }
        settle(&sender);
        drop(player);
    }
}
