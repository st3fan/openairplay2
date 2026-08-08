//! The receiver binary's [`AudioSink`]: ALSA output with a prebuffer cushion
//! and live volume.
//!
//! This is the host side of the sink seam — the library hands it PCM that
//! should play, and it owns the device, the pacing (blocking `writei`), and
//! the gain. The AirPlay volume arrives as an `openairplay2::Event::Volume`
//! in dB; the binary maps it to a linear gain shared with the sink.

use std::ffi::{CStr, CString};
use std::os::raw::{c_int, c_uint, c_void};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use alsa::pcm::PCM;
use alsa::Direction;
use log::{debug, warn};

use openairplay2::AudioSink;

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

/// A human label for an `--alsa-device` value, for the startup log: the sound
/// card's name for a `…CARD=<id>…` device (e.g. "Sound Blaster Play! 2" for
/// `plughw:CARD=S2`), or a fixed label for the well-known plugin PCMs. `None`
/// for anything unrecognized, so the caller logs the bare device.
pub fn card_name(device: &str) -> Option<String> {
    if let Some(id) = card_id_of(device) {
        return alsa::card::Iter::new().flatten().find_map(|card| {
            let info = alsa::ctl::Ctl::from_card(&card, false)
                .and_then(|c| c.card_info())
                .ok()?;
            (info.get_id().ok()? == id).then(|| info.get_name().unwrap_or(&id).to_string())
        });
    }
    let label = match device.split(':').next().unwrap_or(device) {
        "default" => "system default output",
        "sysdefault" => "card default output",
        "pulse" => "PulseAudio",
        "pipewire" => "PipeWire",
        "jack" => "JACK",
        "null" => "discarded",
        _ => return None,
    };
    Some(label.to_string())
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

/// Continue a fade-in in place: ramp `samples` from gain `pos/total` up toward
/// full, one frame at a time, and return the new `pos`. Called per write so a
/// ramp spans several small chunks — a short fade on the first audio after the
/// stream (re)starts or after a flush, so the silence→audio boundary at a
/// track switch is not a click (see plans/20260808-01). `pos >= total` is a
/// no-op (fade complete). Linear is inaudible over a few ms.
fn fade_in_progress(samples: &mut [i16], channels: usize, total: usize, mut pos: usize) -> usize {
    let frames = samples.len() / channels.max(1);
    for f in 0..frames {
        if pos >= total {
            break;
        }
        let g = pos as f32 / total as f32;
        for c in 0..channels {
            let s = &mut samples[f * channels + c];
            *s = (f32::from(*s) * g) as i16;
        }
        pos += 1;
    }
    pos
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
///
/// The PCM stream is opened once and kept **running** for the whole session:
/// it is never dropped or restarted, because stopping and starting the stream
/// makes many DACs mute and un-mute their analog output — an audible pop on
/// every pause, resume and track switch (see plans/20260808-01). When there is
/// nothing to play the device outputs silence (ALSA is configured to fill the
/// buffer with it), and a pause/flush gets immediate silence by *rewinding*
/// the unplayed audio rather than stopping the stream.
pub struct AlsaSink {
    output: Option<AlsaOutput>,
    gain: SharedGain,
    /// Gain-scaled copy of the incoming packet.
    scratch: Vec<i16>,
    channels: usize,
    /// Frames over which to fade the first audio in after a start or a flush.
    fade_len: usize,
    /// Frames faded so far in the current fade-in; `>= fade_len` means done.
    fade_pos: usize,
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
            channels: channels as usize,
            fade_len: (rate / 200) as usize, // ~5 ms
            fade_pos: 0,                     // fade in the first audio too
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
        // Fade the opening frames after a start/flush so the silence→audio
        // boundary is not a click; the ramp continues across chunks.
        if self.fade_pos < self.fade_len {
            self.fade_pos = fade_in_progress(
                &mut self.scratch,
                self.channels,
                self.fade_len,
                self.fade_pos,
            );
        }
        out.write(&self.scratch);
    }

    fn flush(&mut self) {
        // Immediate silence without stopping the stream: rewind the unplayed
        // audio (ALSA fills the gap with silence). Fade the next audio in.
        self.fade_pos = 0;
        if let Some(out) = self.output.as_mut() {
            out.discard();
        }
    }
}

/// Translate an ALSA return code into a message via `snd_strerror`.
fn check(rc: c_int, what: &str) -> Result<(), String> {
    if rc >= 0 {
        return Ok(());
    }
    let msg = unsafe { alsa_sys::snd_strerror(rc) };
    let detail = if msg.is_null() {
        String::new()
    } else {
        unsafe { CStr::from_ptr(msg) }
            .to_string_lossy()
            .into_owned()
    };
    Err(format!("{what}: {detail}"))
}

/// The playback stream, driven directly through `alsa-sys` so it can be kept
/// running (see [`AlsaSink`]) — the safe `alsa` wrapper exposes neither
/// `snd_pcm_rewind` nor the silence sw-params this needs.
struct AlsaOutput {
    pcm: *mut alsa_sys::snd_pcm_t,
    channels: usize,
}

// The player thread owns the sink and is the only user of the handle.
unsafe impl Send for AlsaOutput {}

impl AlsaOutput {
    fn open(device: &str, rate: u32, channels: u8) -> Result<AlsaOutput, String> {
        use alsa_sys as a;
        let name = CString::new(device).map_err(|_| "device name contains NUL".to_string())?;
        unsafe {
            let mut pcm: *mut a::snd_pcm_t = std::ptr::null_mut();
            check(
                a::snd_pcm_open(
                    &mut pcm,
                    name.as_ptr(),
                    a::SND_PCM_STREAM_PLAYBACK as a::snd_pcm_stream_t,
                    0,
                ),
                "snd_pcm_open",
            )?;

            // Hardware params: S16 interleaved at the negotiated rate/channels,
            // ~0.5 s buffer. Read the buffer size back for the sw-params below.
            let mut hwp: *mut a::snd_pcm_hw_params_t = std::ptr::null_mut();
            if a::snd_pcm_hw_params_malloc(&mut hwp) < 0 {
                a::snd_pcm_close(pcm);
                return Err("snd_pcm_hw_params_malloc failed".into());
            }
            let mut buffer_size: a::snd_pcm_uframes_t = 0;
            let hw = (|| {
                check(a::snd_pcm_hw_params_any(pcm, hwp), "hw_params_any")?;
                check(
                    a::snd_pcm_hw_params_set_access(
                        pcm,
                        hwp,
                        a::SND_PCM_ACCESS_RW_INTERLEAVED as a::snd_pcm_access_t,
                    ),
                    "set_access",
                )?;
                check(
                    a::snd_pcm_hw_params_set_format(
                        pcm,
                        hwp,
                        a::SND_PCM_FORMAT_S16_LE as a::snd_pcm_format_t,
                    ),
                    "set_format",
                )?;
                check(
                    a::snd_pcm_hw_params_set_channels(pcm, hwp, c_uint::from(channels)),
                    "set_channels",
                )?;
                let mut r = rate as c_uint;
                check(
                    a::snd_pcm_hw_params_set_rate_near(pcm, hwp, &mut r, std::ptr::null_mut()),
                    "set_rate",
                )?;
                let mut bt: c_uint = 500_000;
                let _ = a::snd_pcm_hw_params_set_buffer_time_near(
                    pcm,
                    hwp,
                    &mut bt,
                    std::ptr::null_mut(),
                );
                check(a::snd_pcm_hw_params(pcm, hwp), "hw_params")?;
                a::snd_pcm_hw_params_get_buffer_size(hwp, &mut buffer_size);
                Ok::<(), String>(())
            })();
            a::snd_pcm_hw_params_free(hwp);
            if let Err(e) = hw {
                a::snd_pcm_close(pcm);
                return Err(e);
            }

            // Software params: never stop on underrun, fill unwritten space
            // with silence (so a gap plays silence, not stale buffer content),
            // and start once the buffer is full. Together these keep the stream
            // running for the whole session.
            let mut swp: *mut a::snd_pcm_sw_params_t = std::ptr::null_mut();
            if a::snd_pcm_sw_params_malloc(&mut swp) < 0 {
                a::snd_pcm_close(pcm);
                return Err("snd_pcm_sw_params_malloc failed".into());
            }
            let sw = (|| {
                check(a::snd_pcm_sw_params_current(pcm, swp), "sw_params_current")?;
                let mut boundary: a::snd_pcm_uframes_t = 0;
                check(
                    a::snd_pcm_sw_params_get_boundary(swp, &mut boundary),
                    "get_boundary",
                )?;
                check(
                    a::snd_pcm_sw_params_set_stop_threshold(pcm, swp, boundary),
                    "set_stop_threshold",
                )?;
                check(
                    a::snd_pcm_sw_params_set_silence_threshold(pcm, swp, 0),
                    "set_silence_threshold",
                )?;
                check(
                    a::snd_pcm_sw_params_set_silence_size(pcm, swp, boundary),
                    "set_silence_size",
                )?;
                let start = if buffer_size > 0 { buffer_size } else { 1 };
                check(
                    a::snd_pcm_sw_params_set_start_threshold(pcm, swp, start),
                    "set_start_threshold",
                )?;
                check(a::snd_pcm_sw_params(pcm, swp), "sw_params")?;
                Ok::<(), String>(())
            })();
            a::snd_pcm_sw_params_free(swp);
            if let Err(e) = sw {
                a::snd_pcm_close(pcm);
                return Err(e);
            }

            if let Err(e) = check(a::snd_pcm_prepare(pcm), "prepare") {
                a::snd_pcm_close(pcm);
                return Err(e);
            }
            Ok(AlsaOutput {
                pcm,
                channels: channels as usize,
            })
        }
    }

    /// Write all interleaved samples, blocking to pace playback. The stream is
    /// started automatically (start-threshold) and, on the rare hard error,
    /// recovered; underruns don't stop it (stop-threshold = boundary).
    fn write(&mut self, samples: &[i16]) {
        use alsa_sys as a;
        let mut off = 0usize;
        unsafe {
            while off < samples.len() {
                let frames = ((samples.len() - off) / self.channels) as a::snd_pcm_uframes_t;
                if frames == 0 {
                    break;
                }
                let n =
                    a::snd_pcm_writei(self.pcm, samples[off..].as_ptr().cast::<c_void>(), frames);
                if n < 0 {
                    if a::snd_pcm_recover(self.pcm, n as c_int, 1) < 0 {
                        warn!("player: unrecoverable ALSA write error");
                        return;
                    }
                    continue; // retry the same frames
                }
                off += n as usize * self.channels;
            }
        }
    }

    /// Immediate silence without stopping the stream: rewind the buffered but
    /// unplayed audio, so the silence-fill takes over from the play position.
    /// Used on pause/flush.
    fn discard(&mut self) {
        use alsa_sys as a;
        unsafe {
            let rewindable = a::snd_pcm_rewindable(self.pcm);
            if rewindable > 0 {
                let _ = a::snd_pcm_rewind(self.pcm, rewindable as a::snd_pcm_uframes_t);
            }
        }
    }
}

impl Drop for AlsaOutput {
    fn drop(&mut self) {
        unsafe {
            alsa_sys::snd_pcm_close(self.pcm);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn fade_in_ramps_across_chunks_and_stops_at_full() {
        // Fade over 6 frames, fed as two 3-frame stereo chunks: the ramp must
        // continue from where the first chunk left off, then pass audio through
        // untouched once complete.
        let mut c1 = vec![1000i16; 6]; // 3 frames
        let pos = fade_in_progress(&mut c1, 2, 6, 0);
        assert_eq!(pos, 3);
        let l1: Vec<i16> = c1.iter().step_by(2).copied().collect();
        assert!(l1[0] < l1[1] && l1[1] < l1[2] && l1[2] < 1000); // rising, sub-full

        let mut c2 = vec![1000i16; 6]; // 3 frames
        let pos = fade_in_progress(&mut c2, 2, 6, pos);
        assert_eq!(pos, 6);
        let l2: Vec<i16> = c2.iter().step_by(2).copied().collect();
        assert!(l2[0] > l1[2]); // continues rising past the first chunk

        // Fade complete: further audio is untouched.
        let mut c3 = vec![1000i16; 4];
        let pos = fade_in_progress(&mut c3, 2, 6, pos);
        assert_eq!(pos, 6);
        assert!(c3.iter().all(|&x| x == 1000));
    }

    #[test]
    fn alsa_output_null_round_trips_the_ffi() {
        // The ALSA "null" device is always present and pure software, so this
        // exercises the raw open → configure → write → discard → write path
        // (catching a broken FFI call here rather than only on real hardware).
        // If the environment has no ALSA config at all, there is nothing to do.
        let Ok(mut out) = AlsaOutput::open("null", 44100, 2) else {
            return;
        };
        out.write(&vec![0i16; 4096]);
        out.discard();
        out.write(&vec![0i16; 4096]);
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

    #[test]
    fn card_name_labels_known_non_card_devices() {
        assert_eq!(
            card_name("default").as_deref(),
            Some("system default output")
        );
        assert_eq!(card_name("pulse").as_deref(), Some("PulseAudio"));
        assert_eq!(card_name("pipewire").as_deref(), Some("PipeWire"));
        assert_eq!(card_name("null").as_deref(), Some("discarded"));
        assert_eq!(card_name("somethingweird"), None);
    }
}
