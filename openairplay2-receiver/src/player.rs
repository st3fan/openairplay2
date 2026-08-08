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

/// The short device list: `default`, then one friendly entry per sound card —
/// the way a desktop's sound settings show "Built-in Audio", "USB DAC", "HDMI"
/// rather than ALSA's wall of plugin definitions.
///
/// Each card becomes `plughw:CARD=<id>` (stable across reboots, and the `plug`
/// layer converts to whatever the hardware wants, so this receiver's 44.1 kHz
/// stereo always plays) labelled with the card's human name. `--list-all-devices`
/// is the escape hatch for a specific sub-device or a raw/plugin PCM.
pub fn list_devices() -> Result<(), alsa::Error> {
    println!("default");
    println!("    System default output");

    // Only cards that actually expose a playback PCM (skip capture-only ones
    // like a webcam mic). Derived from the playback hints, which every
    // playable card contributes a `…:CARD=<id>` entry to.
    let playback = playback_card_ids();

    for card in alsa::card::Iter::new() {
        let card = card?;
        let Ok(info) = alsa::ctl::Ctl::from_card(&card, false).and_then(|c| c.card_info()) else {
            continue;
        };
        let Ok(id) = info.get_id() else { continue };
        if !playback.contains(id) {
            continue;
        }
        let name = info.get_name().unwrap_or(id);
        println!("plughw:CARD={id}");
        println!("    {name}");
    }
    Ok(())
}

/// The full device list: every playback PCM ALSA offers, `aplay -L` style —
/// named devices, hardware sub-devices, and the plugin definitions
/// (`dmix`, `surround*`, converters, `null`, …). For when the short list from
/// [`list_devices`] doesn't name the exact device you need.
pub fn list_all_devices() -> Result<(), alsa::Error> {
    for hint in alsa::device_name::HintIter::new_str(None, "pcm")? {
        // Capture-only devices can never be the output.
        if hint.direction == Some(Direction::Capture) {
            continue;
        }
        let Some(name) = hint.name else { continue };
        println!("{name}");
        if let Some(desc) = hint.desc {
            for line in desc.lines() {
                println!("    {line}");
            }
        }
    }
    Ok(())
}

/// The card ids (`CARD=<id>`) that appear among the playback hints.
fn playback_card_ids() -> std::collections::HashSet<String> {
    let mut ids = std::collections::HashSet::new();
    if let Ok(hints) = alsa::device_name::HintIter::new_str(None, "pcm") {
        for hint in hints {
            if hint.direction == Some(Direction::Capture) {
                continue;
            }
            if let Some(id) = hint.name.as_deref().and_then(card_id_of) {
                ids.insert(id);
            }
        }
    }
    ids
}

/// The card id in a hint or device name — the `<id>` of `CARD=<id>`, e.g.
/// `NVidia` in `plughw:CARD=NVidia,DEV=3`. `None` for names without a card
/// (`default`, `pulse`, `null`).
fn card_id_of(name: &str) -> Option<String> {
    let after = name.split("CARD=").nth(1)?;
    let id: String = after.chars().take_while(|&c| c != ',').collect();
    (!id.is_empty()).then_some(id)
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
    fn card_id_parsing() {
        assert_eq!(
            card_id_of("plughw:CARD=NVidia,DEV=3").as_deref(),
            Some("NVidia")
        );
        assert_eq!(
            card_id_of("sysdefault:CARD=Generic").as_deref(),
            Some("Generic")
        );
        assert_eq!(card_id_of("hw:CARD=S2,DEV=0").as_deref(), Some("S2"));
        // Names without a card have no id.
        assert_eq!(card_id_of("default"), None);
        assert_eq!(card_id_of("pulse"), None);
        assert_eq!(card_id_of("null"), None);
    }
}
