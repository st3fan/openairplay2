//! Validated newtypes shared by the commands: a wire value that carries an
//! invariant worth a name gets a type that upholds it. Normalization happens
//! at construction (part of *reading* the value off the wire); the
//! [`Validate`] impl re-proves the invariant when a params struct embeds the
//! type via `#[validate(nested)]`.

use validator::{Validate, ValidationError, ValidationErrors};

/// The AirPlay volume range: `0` dB is full scale and `-144` is the mute
/// sentinel, so anything outside this says nothing a volume can mean.
pub const MUTE_DB: f32 = -144.0;
pub const FULL_DB: f32 = 0.0;

/// An AirPlay volume in dB, guaranteed finite and within
/// [`MUTE_DB`]`..=`[`FULL_DB`].
///
/// `f32::parse` accepts `nan`, `inf` and overflowing literals like `1e40`,
/// and none of the arithmetic downstream expects them: NaN survives a
/// `min(0.0)` (that returns the *other* operand) and comes out as full
/// scale, which is the loudest possible reading of a value that means
/// nothing. So [`VolumeDb::sanitize`] refuses a non-finite value outright —
/// the knob keeps its old position — and clamps a finite one into the range
/// AirPlay actually uses, which only rewrites values that were already
/// nonsense.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VolumeDb(f32);

impl VolumeDb {
    /// Make a parsed wire value safe to hand to a host's gain path, or
    /// refuse it.
    pub fn sanitize(db: f32) -> Option<VolumeDb> {
        db.is_finite().then(|| VolumeDb(db.clamp(MUTE_DB, FULL_DB)))
    }

    /// Bypass sanitization — for tests proving that validation catches what
    /// construction would have refused.
    #[cfg(test)]
    pub fn new_unchecked(db: f32) -> VolumeDb {
        VolumeDb(db)
    }

    /// The dB value; finite and in range by construction.
    pub fn get(self) -> f32 {
        self.0
    }
}

impl Validate for VolumeDb {
    fn validate(&self) -> Result<(), ValidationErrors> {
        if self.0.is_finite() && (MUTE_DB..=FULL_DB).contains(&self.0) {
            return Ok(());
        }
        let mut errors = ValidationErrors::new();
        let mut error = ValidationError::new("volume_db");
        error.add_param("value".into(), &self.0);
        errors.add("db", error);
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_refuses_non_finite() {
        for db in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert_eq!(VolumeDb::sanitize(db), None, "{db} must be refused");
        }
    }

    #[test]
    fn sanitize_clamps_finite_values_into_the_airplay_range() {
        assert_eq!(VolumeDb::sanitize(6.0), Some(VolumeDb(0.0)));
        assert_eq!(VolumeDb::sanitize(-500.0), Some(VolumeDb(-144.0)));
        assert_eq!(VolumeDb::sanitize(-12.5), Some(VolumeDb(-12.5)));
        assert_eq!(VolumeDb::sanitize(-144.0), Some(VolumeDb(-144.0)));
        assert_eq!(VolumeDb::sanitize(0.0), Some(VolumeDb(0.0)));
    }

    #[test]
    fn validate_catches_what_construction_would_have_refused() {
        // Sanitized values always validate...
        assert!(VolumeDb::sanitize(-12.5).unwrap().validate().is_ok());
        // ...and an unchecked value that skipped sanitization does not.
        for db in [f32::NAN, f32::INFINITY, 6.0, -500.0] {
            assert!(
                VolumeDb::new_unchecked(db).validate().is_err(),
                "{db} must fail validation"
            );
        }
    }
}
