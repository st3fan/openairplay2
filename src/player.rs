//! ALSA audio output — a dedicated OS thread that plays interleaved `i16` PCM.
//!
//! Simplified from the AirPlay 1 receiver's player: a fixed prebuffer then
//! blocking writes (which pace playback). PTP-accurate start and drift
//! correction are milestone 6; for now this buffers and plays.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;

use alsa::pcm::{Access, Format, HwParams, State, PCM};
use alsa::{Direction, ValueOr};
use log::{debug, info, warn};

/// Prebuffer this many decoded frames (packets) before the first ALSA write.
const PREBUFFER_PACKETS: usize = 20;

enum Command {
    Pcm(Vec<i16>),
    Stop,
}

/// A cloneable handle for feeding decoded PCM to the playback thread.
#[derive(Clone)]
pub struct PlayerSender {
    tx: Sender<Command>,
}

impl PlayerSender {
    pub fn play(&self, pcm: Vec<i16>) {
        let _ = self.tx.send(Command::Pcm(pcm));
    }
}

/// Owns the playback thread; dropping it stops the thread and closes the device.
pub struct Player {
    tx: Option<Sender<Command>>,
    handle: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

impl Player {
    /// Spawn the playback thread. `device` is an ALSA device name, or `None`
    /// for decode-only (no audio). Never fails: a device that won't open is
    /// logged and audio is discarded so the session keeps running.
    pub fn spawn(sample_rate: u32, channels: u8, device: Option<String>) -> Player {
        let (tx, rx) = std::sync::mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = stop.clone();
        let handle = std::thread::Builder::new()
            .name("alsa-player".into())
            .spawn(move || run(sample_rate, channels, device, rx, thread_stop))
            .expect("spawn player thread");
        Player {
            tx: Some(tx),
            handle: Some(handle),
            stop,
        }
    }

    pub fn sender(&self) -> PlayerSender {
        PlayerSender {
            tx: self.tx.clone().expect("sender available before drop"),
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

    let mut prebuffer: Vec<i16> = Vec::new();
    let mut started = false;
    let mut packets: u64 = 0;
    while let Ok(command) = rx.recv() {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let pcm = match command {
            Command::Pcm(pcm) => pcm,
            Command::Stop => break,
        };
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
            if packets as usize >= PREBUFFER_PACKETS {
                started = true;
                out.write(&prebuffer);
                prebuffer.clear();
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
}
