//! The receiver binary's [`AudioSink`]: ALSA output with a prebuffer cushion
//! and live volume.
//!
//! This is the host side of the sink seam — the library hands it PCM that
//! should play, and it owns the device, the pacing (blocking `writei`), and
//! the gain. The AirPlay volume arrives as an `openairplay2::Event::Volume`
//! in dB; the binary maps it to a linear gain shared with the sink.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use alsa::pcm::{Access, Format, HwParams, State, PCM};
use alsa::{Direction, ValueOr};
use log::{debug, warn};

use openairplay2::AudioSink;

/// Prebuffer this fraction of a second before the first ALSA write.
fn start_samples(rate: u32, channels: u8) -> usize {
    (rate as usize / 2) * channels as usize // ~0.5 s
}

/// What a startup probe of the configured device means.
#[derive(Debug)]
pub enum Probe {
    /// The device opened; it exists and we may use it.
    Ok,
    /// Could not open it *now*, which says nothing about stream time (busy,
    /// or something unclassified): log and start anyway.
    Warn(alsa::Error),
    /// The configuration is wrong (no such device, no permission) and no
    /// amount of waiting fixes it: refuse to start, with the fix in the
    /// message.
    Fatal(String),
}

/// Open the device once and drop it, so a wrong `--alsa-device` fails at
/// startup, in the user's face — not as a receiver that decodes to nowhere.
pub fn probe_device(device: &str) -> Probe {
    match PCM::new(device, Direction::Playback, false) {
        Ok(_) => Probe::Ok,
        Err(e) => triage(device, e),
    }
}

/// `alsa::Error::errno` is negative when it comes straight from a C return
/// value and positive when it comes from OS errno, hence the `abs`.
fn triage(device: &str, e: alsa::Error) -> Probe {
    match e.errno().abs() {
        libc::ENOENT | libc::ENODEV => Probe::Fatal(format!(
            "ALSA device \"{device}\" does not exist ({e}); \
             run `openairplay2-receiver --list-devices` to see this machine's playback devices"
        )),
        libc::EACCES | libc::EPERM => Probe::Fatal(format!(
            "no permission to open ALSA device \"{device}\" ({e}); \
             is this user in the `audio` group?"
        )),
        _ => Probe::Warn(e),
    }
}

/// Whether a device name is worth offering as an `--alsa-device` target.
///
/// ALSA's hint list is dominated by plugin definitions — the null sink,
/// software mixers (`dmix`/`dsnoop`), channel-layout plugins (`surround*`,
/// `front`, …) and rate/channel converters — that are either irrelevant to a
/// plain stereo receiver or a worse pick than the card's own
/// `default`/`plughw`/`hw`/`sysdefault` and its digital/HDMI outputs. Keep the
/// real outputs, drop the plumbing. This is a denylist by plugin family (the
/// token before the first `:`), so an unfamiliar real device is still shown.
fn is_listable_device(name: &str) -> bool {
    let family = name.split(':').next().unwrap_or(name);
    const DROP_EXACT: &[&str] = &[
        "null",
        "front",
        "rear",
        "side",
        "center_lfe",
        "oss",
        "modem",
        "phoneline",
    ];
    const DROP_PREFIX: &[&str] = &[
        "dmix",       // shared software mixing — the default already uses it
        "dsnoop",     // shared capture
        "dshare",     // shared output plumbing
        "surround",   // multichannel profiles; this receiver is stereo
        "samplerate", // rate converters
        "speexrate",
        "lavrate",
        "upmix",     // channel converters
        "vdownmix",  //
        "usbstream", // raw USB gadget stream
    ];
    !DROP_EXACT.contains(&family) && !DROP_PREFIX.iter().any(|p| family.starts_with(p))
}

/// Print the playback devices, `aplay -L` style: the name to give
/// `--alsa-device`, with ALSA's description indented beneath it. Plugin
/// definitions that aren't real outputs are filtered out — see
/// [`is_listable_device`].
pub fn list_devices() -> Result<(), alsa::Error> {
    for hint in alsa::device_name::HintIter::new_str(None, "pcm")? {
        // Capture-only devices can never be the output.
        if hint.direction == Some(Direction::Capture) {
            continue;
        }
        let Some(name) = hint.name else { continue };
        if !is_listable_device(&name) {
            continue;
        }
        println!("{name}");
        if let Some(desc) = hint.desc {
            for line in desc.lines() {
                println!("    {line}");
            }
        }
    }
    Ok(())
}

/// Convert an AirPlay volume in dB to a linear gain in `[0, 1]`. `0 dB` is full
/// volume, `-144 dB` (and below) is muted; we never amplify above unity.
pub fn volume_to_gain(db: f32) -> f32 {
    if db <= -144.0 {
        return 0.0;
    }
    10f32.powf(db.min(0.0) / 20.0)
}

/// Scale interleaved samples by a linear gain in `[0, 1]`. Full gain is a
/// no-op; zero is silence. Gain ≤ 1 so the product stays in range.
fn apply_gain(samples: &mut [i16], gain: f32) {
    if gain >= 0.999 {
        return;
    }
    if gain <= 0.0 {
        samples.fill(0);
        return;
    }
    for s in samples.iter_mut() {
        *s = (f32::from(*s) * gain) as i16;
    }
}

/// The playback gain, shared between the event consumer (which sets it from
/// volume events) and the sink (which applies it live). Outlives any single
/// stream, so a volume set before `SETUP` phase 2 isn't lost.
#[derive(Clone)]
pub struct SharedGain(Arc<AtomicU32>);

impl SharedGain {
    /// Starts at full volume.
    pub fn new() -> SharedGain {
        SharedGain(Arc::new(AtomicU32::new(1.0f32.to_bits())))
    }

    /// Set the linear gain (`1.0` = full, `0.0` = mute).
    pub fn set(&self, gain: f32) {
        self.0
            .store(gain.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    pub fn get(&self) -> f32 {
        f32::from_bits(self.0.load(Ordering::Relaxed))
    }
}

impl Default for SharedGain {
    fn default() -> SharedGain {
        SharedGain::new()
    }
}

/// Discards all audio — used for `--no-audio` (decode-only). Never blocks,
/// so the pipeline runs unpaced, same as before the sink seam existed.
pub struct NullSink;

impl AudioSink for NullSink {
    fn write(&mut self, _pcm: &[i16]) {}
    fn flush(&mut self) {}
}

/// ALSA playback sink. A device that won't open is logged and audio is
/// discarded so the session keeps running.
pub struct AlsaSink {
    output: Option<AlsaOutput>,
    gain: SharedGain,
    /// Gain-scaled copy of the incoming packet.
    scratch: Vec<i16>,
    /// Cushion accumulated before the first write so startup doesn't underrun.
    prebuffer: Vec<i16>,
    threshold: usize,
    started: bool,
}

impl AlsaSink {
    pub fn open(device: &str, rate: u32, channels: u8, gain: SharedGain) -> AlsaSink {
        let output = match AlsaOutput::open(device, rate, channels) {
            Ok(out) => {
                debug!("player: ALSA \"{device}\" {rate} Hz {channels}ch");
                Some(out)
            }
            Err(e) => {
                warn!("player: cannot open ALSA ({e}); decode-only");
                None
            }
        };
        AlsaSink {
            output,
            gain,
            scratch: Vec::new(),
            prebuffer: Vec::new(),
            threshold: start_samples(rate, channels),
            started: false,
        }
    }
}

impl AudioSink for AlsaSink {
    fn write(&mut self, pcm: &[i16]) {
        let Some(out) = self.output.as_mut() else {
            return; // device failed to open → discard
        };
        self.scratch.clear();
        self.scratch.extend_from_slice(pcm);
        // Apply the current volume (live) just before playback.
        apply_gain(&mut self.scratch, self.gain.get());
        if self.started {
            out.write(&self.scratch);
        } else {
            self.prebuffer.extend_from_slice(&self.scratch);
            if self.prebuffer.len() >= self.threshold {
                self.started = true;
                out.write(&self.prebuffer);
                self.prebuffer.clear();
            }
        }
    }

    fn flush(&mut self) {
        self.prebuffer.clear();
        self.started = false;
        if let Some(out) = self.output.as_mut() {
            out.reset(); // discard queued frames → immediate silence
        }
    }
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
    /// audio. Used on pause/flush. Unlike `snd_pcm_pause`, `drop` + `prepare`
    /// are supported everywhere (`snd_pcm_pause` fails with EBADFD on `front`).
    fn reset(&mut self) {
        let _ = self.pcm.drop();
        let _ = self.pcm.prepare();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prebuffer_threshold_for_44100_stereo() {
        assert_eq!(start_samples(44100, 2), 44100); // 0.5 s interleaved
    }

    #[test]
    fn apply_gain_scales_samples() {
        // Half gain halves amplitude.
        let mut s = [100i16, -100, 20000, -20000];
        apply_gain(&mut s, 0.5);
        assert_eq!(s, [50, -50, 10000, -10000]);

        // Zero gain → silence.
        let mut s = [1i16, -32768, 32767];
        apply_gain(&mut s, 0.0);
        assert_eq!(s, [0, 0, 0]);

        // Full gain leaves samples untouched (and doesn't clip the extremes).
        let mut s = [1i16, -32768, 32767];
        apply_gain(&mut s, 1.0);
        assert_eq!(s, [1, -32768, 32767]);
    }

    #[test]
    fn volume_db_to_gain() {
        assert!((volume_to_gain(0.0) - 1.0).abs() < 1e-6); // full
        assert!((volume_to_gain(-6.0206) - 0.5).abs() < 1e-3); // −6 dB ≈ half
        assert!((volume_to_gain(-20.0) - 0.1).abs() < 1e-4); // −20 dB = 0.1
        assert_eq!(volume_to_gain(-144.0), 0.0); // muted
        assert_eq!(volume_to_gain(-200.0), 0.0); // below sentinel = muted
        assert!((volume_to_gain(6.0) - 1.0).abs() < 1e-6); // never amplify

        // The shared gain clamps and round-trips.
        let gain = SharedGain::new();
        assert_eq!(gain.get(), 1.0);
        gain.set(volume_to_gain(-20.0));
        assert!((gain.get() - 0.1).abs() < 1e-4);
        gain.set(2.0);
        assert_eq!(gain.get(), 1.0);
    }

    #[test]
    fn triage_not_found_is_fatal_and_points_at_list_devices() {
        // snd_pcm_open returns negative errnos (alsa::Error::from_code).
        for errno in [-libc::ENOENT, -libc::ENODEV] {
            let probe = triage("nonsense", alsa::Error::new("snd_pcm_open", errno));
            match probe {
                Probe::Fatal(msg) => {
                    assert!(msg.contains("nonsense"), "{msg}");
                    assert!(msg.contains("--list-devices"), "{msg}");
                }
                other => panic!("expected Fatal, got {other:?}"),
            }
        }
    }

    #[test]
    fn triage_permission_is_fatal_and_names_the_audio_group() {
        for errno in [-libc::EACCES, -libc::EPERM] {
            let probe = triage("default", alsa::Error::new("snd_pcm_open", errno));
            match probe {
                Probe::Fatal(msg) => assert!(msg.contains("audio"), "{msg}"),
                other => panic!("expected Fatal, got {other:?}"),
            }
        }
    }

    #[test]
    fn triage_busy_and_the_unclassified_warn_and_start() {
        // Positive errnos too — alsa::Error::last stores them unnegated.
        for errno in [-libc::EBUSY, libc::EBUSY, -libc::EIO] {
            let probe = triage("default", alsa::Error::new("snd_pcm_open", errno));
            assert!(matches!(probe, Probe::Warn(_)), "errno {errno}: {probe:?}");
        }
    }

    #[test]
    fn list_filter_keeps_real_outputs_and_drops_plumbing() {
        for keep in [
            "default",
            "pipewire",
            "pulse",
            "jack",
            "sysdefault:CARD=Generic",
            "hw:CARD=S2,DEV=0",
            "plughw:CARD=S2,DEV=0",
            "hdmi:CARD=NVidia,DEV=0",
            "iec958:CARD=Generic,DEV=0",
        ] {
            assert!(is_listable_device(keep), "should keep {keep}");
        }
        for drop in [
            "null",
            "dmix:CARD=NVidia,DEV=3",
            "dsnoop:CARD=S2,DEV=0",
            "surround51:CARD=Generic,DEV=0",
            "surround21:CARD=S2,DEV=0",
            "front:CARD=S2,DEV=0",
            "samplerate",
            "speexrate",
            "upmix",
            "vdownmix",
            "usbstream",
        ] {
            assert!(!is_listable_device(drop), "should drop {drop}");
        }
    }
}
