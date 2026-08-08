//! Hardware mixer volume: drive an ALSA mixer control from AirPlay volume
//! events instead of scaling PCM samples in software
//! (plans/20260808-02-hardware-mixer-volume.md). Software gain throws away
//! bits at low volume; a card that attenuates in its analog stage keeps the
//! full 16 all the way down, and a USB DAC or amp HAT with its own volume
//! control should have *that* control follow the sender's slider.

use alsa::mixer::{MilliBel, Mixer, Selem, SelemId};
use alsa::Round;
use log::{debug, warn};

use crate::player::SharedGain;

/// The AirPlay mute sentinel. The slider itself never goes below
/// [`SLIDER_FLOOR_DB`]; `-144` means "muted", not "very quiet".
const MUTE_DB: f32 = -144.0;

/// The bottom of the AirPlay volume slider; the top is 0 dB.
const SLIDER_FLOOR_DB: f32 = -30.0;

/// A playback mixer control being driven by the sender's volume.
///
/// The slider's `[-30, 0]` dB is an AirPlay constant, not something the
/// listener chose, so it is mapped linearly in dB onto the control's own
/// range — slider-top = the control's maximum, slider-bottom = its minimum,
/// the taper in between is the control's own dB scale (the same mapping
/// shairport-sync uses). Passing the dB through unscaled would strand
/// everything below −30 dB on a control with a 100 dB range.
pub struct HwVolume {
    mixer: Mixer,
    id: SelemId,
    /// For log and error messages.
    device: String,
    control: String,
    has_switch: bool,
    /// `(min, max)` in millibels; `None` for a control without a dB scale,
    /// which falls back to the raw volume range below.
    db_range: Option<(i64, i64)>,
    raw_range: (i64, i64),
    /// The software gain, parked at full scale while hardware volume is
    /// active. Touched only to guarantee silence when muting a control that
    /// has no switch — whose minimum may be quiet, not silent.
    gain: SharedGain,
}

impl HwVolume {
    /// Open `control` (a simple-mixer control name, optionally `NAME,INDEX`)
    /// on mixer device `device`. Errors are startup-fatal config mistakes;
    /// the message carries the fix.
    pub fn open(device: &str, control: &str, gain: SharedGain) -> Result<HwVolume, String> {
        let mixer = Mixer::new(device, false)
            .map_err(|e| format!("cannot open mixer device \"{device}\" ({e})"))?;
        let (name, index) = control_id(control);
        let id = SelemId::new(name, index);
        let Some(selem) = mixer.find_selem(&id) else {
            return Err(format!(
                "mixer device \"{device}\" has no control \"{control}\"; \
                 run `openairplay2-receiver --list-mixers` to see the controls"
            ));
        };
        if !selem.has_playback_volume() {
            return Err(format!(
                "mixer control \"{control}\" on \"{device}\" has no playback volume; \
                 run `openairplay2-receiver --list-mixers` to see the controls"
            ));
        }
        let has_switch = selem.has_playback_switch();
        let db_range = db_range(&selem);
        let raw_range = selem.get_playback_volume_range();
        if db_range.is_none() && raw_range.0 >= raw_range.1 {
            return Err(format!(
                "mixer control \"{control}\" on \"{device}\" reports no usable volume range"
            ));
        }
        Ok(HwVolume {
            mixer,
            id,
            device: device.to_string(),
            control: control.to_string(),
            has_switch,
            db_range,
            raw_range,
            gain,
        })
    }

    /// The control and its range, for the startup log.
    pub fn describe(&self) -> String {
        let range = match self.db_range {
            Some((min, max)) => format!(
                "{:.2}..{:.2} dB",
                MilliBel(min).to_db(),
                MilliBel(max).to_db()
            ),
            None => format!(
                "raw {}..{} (no dB scale)",
                self.raw_range.0, self.raw_range.1
            ),
        };
        format!("control \"{}\" on {} ({range})", self.control, self.device)
    }

    /// Apply a sender volume (dB, `-144` = mute) to the control. A set that
    /// fails is logged and dropped — audio keeps playing at the old level.
    pub fn set(&mut self, db: f32) {
        let Some(selem) = self.mixer.find_selem(&self.id) else {
            warn!(
                "mixer: control \"{}\" disappeared from {}",
                self.control, self.device
            );
            return;
        };
        let result = if db <= MUTE_DB {
            self.gain.set(if self.has_switch { 1.0 } else { 0.0 });
            debug!("mixer: mute");
            mute(&selem, self.has_switch, self.db_range, self.raw_range)
        } else {
            // Restores the belt-and-braces mute below; otherwise a no-op.
            self.gain.set(1.0);
            match self.db_range {
                Some((min, max)) => {
                    let mb = slider_to_range(db, min, max);
                    debug!("mixer: {db} dB → {:.2} dB", MilliBel(mb).to_db());
                    unmute_and_set_db(&selem, self.has_switch, mb)
                }
                None => {
                    let raw = slider_to_range(db, self.raw_range.0, self.raw_range.1);
                    debug!("mixer: {db} dB → raw {raw}");
                    unmute_and_set_raw(&selem, self.has_switch, raw)
                }
            }
        };
        if let Err(e) = result {
            warn!(
                "mixer: cannot set \"{}\" on {} ({e})",
                self.control, self.device
            );
        }
    }
}

/// Mute: prefer the control's own switch; without one, the control's minimum
/// (the caller has already zeroed the software gain, because a minimum like
/// −60 dB is quiet, not silent).
fn mute(
    selem: &Selem,
    has_switch: bool,
    db_range: Option<(i64, i64)>,
    raw_range: (i64, i64),
) -> alsa::Result<()> {
    if has_switch {
        return selem.set_playback_switch_all(0);
    }
    match db_range {
        Some((min, _)) => selem.set_playback_db_all(MilliBel(min), Round::Floor),
        None => selem.set_playback_volume_all(raw_range.0),
    }
}

fn unmute_and_set_db(selem: &Selem, has_switch: bool, mb: i64) -> alsa::Result<()> {
    selem.set_playback_db_all(MilliBel(mb), Round::Floor)?;
    if has_switch {
        selem.set_playback_switch_all(1)?;
    }
    Ok(())
}

fn unmute_and_set_raw(selem: &Selem, has_switch: bool, raw: i64) -> alsa::Result<()> {
    selem.set_playback_volume_all(raw)?;
    if has_switch {
        selem.set_playback_switch_all(1)?;
    }
    Ok(())
}

/// The control's dB range in millibels, `None` when it reports none (ALSA
/// leaves the out-params zeroed on error, so a degenerate range means "no dB
/// scale" — fall back to the raw volume steps).
fn db_range(selem: &Selem) -> Option<(i64, i64)> {
    let (min, max) = selem.get_playback_db_range();
    (min.0 < max.0).then_some((min.0, max.0))
}

/// Split an optional `,INDEX` suffix off a control argument: `"Speaker,1"`
/// selects index 1; anything whose tail is not a number is all name.
fn control_id(control: &str) -> (&str, u32) {
    if let Some((name, index)) = control.rsplit_once(',') {
        if !index.is_empty() && index.bytes().all(|b| b.is_ascii_digit()) {
            if let Ok(index) = index.parse() {
                return (name, index);
            }
        }
    }
    (control, 0)
}

/// Where the slider sits in its travel: 0.0 at the floor, 1.0 at the top.
fn slider_position(db: f32) -> f64 {
    f64::from(((db - SLIDER_FLOOR_DB) / -SLIDER_FLOOR_DB).clamp(0.0, 1.0))
}

/// Map the slider onto a control range — millibels or raw steps, whatever
/// `(min, max)` is in — rounding to the nearest step.
fn slider_to_range(db: f32, min: i64, max: i64) -> i64 {
    min + ((max - min) as f64 * slider_position(db)).round() as i64
}

/// `--list-mixers`: the playback volume controls of the common mixer devices
/// — `default` (where PulseAudio/PipeWire expose their `Master`) and each
/// sound card — with their ranges, for picking a `--mixer` value. A device
/// whose mixer won't open is skipped, same as a card without controls.
pub fn list_mixers() -> Result<(), alsa::Error> {
    print_controls("default");
    for card in alsa::card::Iter::new() {
        let card = card?;
        let Ok(info) = alsa::ctl::Ctl::from_card(&card, false).and_then(|c| c.card_info()) else {
            continue;
        };
        let Ok(id) = info.get_id() else { continue };
        print_controls(&format!("hw:CARD={id}"));
    }
    Ok(())
}

/// Print one mixer device and its playback volume controls, `--list-devices`
/// style. Nothing at all for a device with no such controls.
fn print_controls(device: &str) {
    let Ok(mixer) = Mixer::new(device, false) else {
        return;
    };
    let mut header = false;
    for selem in mixer.iter().filter_map(Selem::new) {
        if !selem.has_playback_volume() {
            continue;
        }
        if !header {
            println!("{device}");
            header = true;
        }
        let id = selem.get_id();
        let name = match id.get_index() {
            0 => id.get_name().unwrap_or("?").to_string(),
            index => format!("{},{index}", id.get_name().unwrap_or("?")),
        };
        let range = match db_range(&selem) {
            Some((min, max)) => format!(
                "{:.2}..{:.2} dB",
                MilliBel(min).to_db(),
                MilliBel(max).to_db()
            ),
            None => {
                let (min, max) = selem.get_playback_volume_range();
                format!("raw {min}..{max}, no dB scale")
            }
        };
        let switch = if selem.has_playback_switch() {
            ", switch"
        } else {
            ""
        };
        println!("    {name}  ({range}{switch})");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slider_maps_onto_the_control_range() {
        // A typical −102.4..0 dB control, in millibels.
        let (min, max) = (-10240, 0);
        assert_eq!(slider_to_range(0.0, min, max), max); // slider top
        assert_eq!(slider_to_range(-30.0, min, max), min); // slider bottom
        assert_eq!(slider_to_range(-15.0, min, max), -5120); // midpoint

        // Out-of-range input clamps to the endpoints; −144 is the mute
        // sentinel and never reaches this mapping, but clamping still holds.
        assert_eq!(slider_to_range(6.0, min, max), max);
        assert_eq!(slider_to_range(-90.0, min, max), min);
    }

    #[test]
    fn slider_maps_onto_a_raw_range_too() {
        // A control without a dB scale: raw steps 0..255.
        assert_eq!(slider_to_range(0.0, 0, 255), 255);
        assert_eq!(slider_to_range(-30.0, 0, 255), 0);
        assert_eq!(slider_to_range(-15.0, 0, 255), 128); // rounds to nearest
    }

    #[test]
    fn slider_survives_a_degenerate_range() {
        // min == max must not divide by zero or leave the range.
        assert_eq!(slider_to_range(-15.0, -600, -600), -600);
    }

    #[test]
    fn control_argument_takes_an_optional_index() {
        assert_eq!(control_id("PCM"), ("PCM", 0));
        assert_eq!(control_id("Master"), ("Master", 0));
        assert_eq!(control_id("Speaker,1"), ("Speaker", 1));
        // A tail that is not a number is part of the name, as is a trailing
        // comma or an index that overflows.
        assert_eq!(control_id("Foo,Bar"), ("Foo,Bar", 0));
        assert_eq!(control_id("Foo,"), ("Foo,", 0));
        assert_eq!(control_id("Foo,99999999999"), ("Foo,99999999999", 0));
    }

    #[test]
    fn no_db_scale_is_a_degenerate_range() {
        // ALSA zeroes the out-params for a control without dB info, so the
        // detection must treat min >= max as "no dB scale". Exercised through
        // slider_to_range on the raw range in that case (see HwVolume::set);
        // here just pin the position math the detection relies on.
        assert_eq!(slider_position(0.0), 1.0);
        assert_eq!(slider_position(-30.0), 0.0);
        assert_eq!(slider_position(-144.0), 0.0); // clamped, not negative
    }
}
