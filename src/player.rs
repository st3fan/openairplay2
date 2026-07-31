//! ALSA audio output — a dedicated OS thread that plays interleaved `i16` PCM.
//!
//! Timing is deliberately "soft" (no PTP): the Mac buffers audio ahead and we
//! drain it at the sound card's rate. The playback thread prebuffers a cushion,
//! then blocking writes pace playback; the TCP reader backpressures on the
//! queued-sample count so latency and memory stay bounded.
//!
//! Transport control (pause/seek) is **out-of-band** from the audio queue: a
//! flush bumps a generation counter that the control path can set instantly,
//! and every queued packet is stamped with the generation it was produced
//! under. On a flush the player drops all stale-stamped packets (microseconds,
//! not played) and resets the device — so control isn't stuck behind the ~2 s
//! audio buffer. (An in-band command would sit behind that buffer and only act
//! seconds later.)

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;

use alsa::pcm::{Access, Format, HwParams, State, PCM};
use alsa::{Direction, ValueOr};
use log::{debug, info, warn};

/// Prebuffer this fraction of a second before the first ALSA write.
fn start_samples(rate: u32, channels: u8) -> usize {
    (rate as usize / 2) * channels as usize // ~0.5 s
}

/// Queue high-water mark (interleaved samples): above this the reader
/// backpressures so latency/memory stay bounded. ~2 s of audio.
pub fn max_queued_samples(rate: u32, channels: u8) -> usize {
    rate as usize * channels as usize * 2
}

enum Command {
    /// (flush generation the packet was produced under, interleaved samples).
    Pcm(u64, Vec<i16>),
    /// Nudge the loop to re-check the flush generation (used when the queue is
    /// idle, e.g. paused, so a flush is still noticed promptly).
    Wake,
    Stop,
}

/// A cloneable handle for feeding decoded PCM to the playback thread and
/// flushing it (pause/seek).
#[derive(Clone)]
pub struct PlayerSender {
    tx: Sender<Command>,
    /// Interleaved samples sent but not yet taken by the playback thread.
    pending: Arc<AtomicUsize>,
    /// Bumped on every flush; stamps outgoing audio so stale audio is dropped.
    flush_gen: Arc<AtomicU64>,
}

impl PlayerSender {
    pub fn play(&self, pcm: Vec<i16>) {
        let gen = self.flush_gen.load(Ordering::Relaxed);
        self.pending.fetch_add(pcm.len(), Ordering::Relaxed);
        let _ = self.tx.send(Command::Pcm(gen, pcm));
    }

    /// Drop all buffered audio immediately (pause/seek/skip). Bumps the
    /// generation so already-queued audio is discarded, and wakes the thread
    /// in case it is idle.
    pub fn flush(&self) {
        self.flush_gen.fetch_add(1, Ordering::Relaxed);
        let _ = self.tx.send(Command::Wake);
    }

    /// Interleaved samples currently queued — the backpressure signal.
    pub fn pending_samples(&self) -> usize {
        self.pending.load(Ordering::Relaxed)
    }

    /// Current flush generation (for tests/inspection).
    pub fn generation(&self) -> u64 {
        self.flush_gen.load(Ordering::Relaxed)
    }
}

/// Owns the playback thread; dropping it stops the thread and closes the device.
pub struct Player {
    tx: Option<Sender<Command>>,
    handle: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    pending: Arc<AtomicUsize>,
    flush_gen: Arc<AtomicU64>,
}

impl Player {
    /// Spawn the playback thread. `device` is an ALSA device name, or `None`
    /// for decode-only (no audio). Never fails: a device that won't open is
    /// logged and audio is discarded so the session keeps running.
    pub fn spawn(sample_rate: u32, channels: u8, device: Option<String>) -> Player {
        let (tx, rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let pending = Arc::new(AtomicUsize::new(0));
        let flush_gen = Arc::new(AtomicU64::new(0));
        let thread_stop = stop.clone();
        let thread_pending = pending.clone();
        let thread_gen = flush_gen.clone();
        let handle = std::thread::Builder::new()
            .name("alsa-player".into())
            .spawn(move || {
                run(
                    sample_rate,
                    channels,
                    device,
                    rx,
                    thread_stop,
                    thread_pending,
                    thread_gen,
                )
            })
            .expect("spawn player thread");
        Player {
            tx: Some(tx),
            handle: Some(handle),
            stop,
            pending,
            flush_gen,
        }
    }

    pub fn sender(&self) -> PlayerSender {
        PlayerSender {
            tx: self.tx.clone().expect("sender available before drop"),
            pending: self.pending.clone(),
            flush_gen: self.flush_gen.clone(),
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

fn run(
    sample_rate: u32,
    channels: u8,
    device: Option<String>,
    rx: Receiver<Command>,
    stop: Arc<AtomicBool>,
    pending: Arc<AtomicUsize>,
    flush_gen: Arc<AtomicU64>,
) {
    let mut output = match device {
        Some(name) => match AlsaOutput::open(&name, sample_rate, channels) {
            Ok(out) => {
                info!("player: ALSA \"{name}\" {sample_rate} Hz {channels}ch");
                Some(out)
            }
            Err(e) => {
                warn!("player: cannot open ALSA ({e}); decode-only");
                None
            }
        },
        None => {
            info!("player: --no-audio, decode-only");
            None
        }
    };

    let threshold = start_samples(sample_rate, channels);
    let mut prebuffer: Vec<i16> = Vec::new();
    let mut started = false;
    let mut cur_gen: u64 = 0;
    let mut packets: u64 = 0;
    while let Ok(command) = rx.recv() {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        // React to any flush (set out-of-band by the control path) before
        // touching audio, so control isn't stuck behind the buffer.
        let gen = flush_gen.load(Ordering::Relaxed);
        if gen != cur_gen {
            cur_gen = gen;
            prebuffer.clear();
            started = false;
            if let Some(out) = output.as_mut() {
                out.reset();
            }
            debug!("player: flushed (gen {gen})");
        }
        match command {
            Command::Stop => break,
            Command::Wake => {}
            Command::Pcm(stamp, pcm) => {
                pending.fetch_sub(pcm.len(), Ordering::Relaxed);
                if stamp != cur_gen {
                    continue; // produced before a flush → stale, drop it
                }
                packets += 1;
                if packets <= 3 || packets.is_multiple_of(250) {
                    debug!("player: {packets} packets");
                }
                let Some(out) = output.as_mut() else {
                    continue; // decode-only
                };
                if started {
                    out.write(&pcm);
                } else {
                    prebuffer.extend_from_slice(&pcm);
                    if prebuffer.len() >= threshold {
                        started = true;
                        out.write(&prebuffer);
                        prebuffer.clear();
                    }
                }
            }
        }
    }
    debug!("player: stopped, {packets} packets");
}

struct AlsaOutput {
    pcm: PCM,
    channels: usize,
}

impl AlsaOutput {
    fn open(device: &str, rate: u32, channels: u8) -> Result<AlsaOutput, alsa::Error> {
        let pcm = PCM::new(device, Direction::Playback, false)?;
        {
            let hwp = HwParams::any(&pcm)?;
            hwp.set_channels(u32::from(channels))?;
            hwp.set_rate(rate, ValueOr::Nearest)?;
            hwp.set_format(Format::s16())?;
            hwp.set_access(Access::RWInterleaved)?;
            let _ = hwp.set_buffer_time_near(500_000, ValueOr::Nearest);
            pcm.hw_params(&hwp)?;
        }
        pcm.prepare()?;
        Ok(AlsaOutput {
            pcm,
            channels: channels as usize,
        })
    }

    /// Write all interleaved samples, blocking to pace playback and recovering
    /// from underruns.
    fn write(&mut self, samples: &[i16]) {
        let Ok(io) = self.pcm.io_i16() else {
            warn!("player: ALSA io handle lost");
            return;
        };
        let mut frames = samples;
        while !frames.is_empty() {
            match io.writei(frames) {
                Ok(0) => break,
                Ok(written) => frames = &frames[written * self.channels..],
                Err(e) => {
                    if self.pcm.try_recover(e, true).is_err() {
                        warn!("player: unrecoverable ALSA write error");
                        return;
                    }
                }
            }
        }
        if self.pcm.state() != State::Running {
            let _ = self.pcm.start();
        }
    }

    /// Discard queued frames (immediate silence) and ready the device for new
    /// audio. Used on flush (pause/seek). Unlike `snd_pcm_pause`, `drop` +
    /// `prepare` are supported everywhere.
    fn reset(&mut self) {
        let _ = self.pcm.drop();
        let _ = self.pcm.prepare();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thresholds_for_44100_stereo() {
        assert_eq!(start_samples(44100, 2), 44100); // 0.5 s interleaved
        assert_eq!(max_queued_samples(44100, 2), 176_400); // 2 s interleaved
    }

    #[test]
    fn flush_bumps_generation() {
        let player = Player::spawn(44100, 2, None);
        let sender = player.sender();
        assert_eq!(sender.generation(), 0);
        sender.flush();
        assert_eq!(sender.generation(), 1);
        sender.flush();
        assert_eq!(sender.generation(), 2);
    }

    #[test]
    fn play_then_consume_tracks_pending() {
        // With no device the thread drains commands; after it settles, the
        // pending counter returns to zero.
        let player = Player::spawn(44100, 2, None);
        let sender = player.sender();
        for _ in 0..10 {
            sender.play(vec![0i16; 2048]);
        }
        // Let the decode-only thread consume the queue (bounded wait).
        for _ in 0..400 {
            if sender.pending_samples() == 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(sender.pending_samples(), 0);
    }
}
