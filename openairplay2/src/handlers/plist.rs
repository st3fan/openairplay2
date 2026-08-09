//! Small plist extraction helpers shared by the RTSP handlers — the
//! "extractor" analog for a wire format no framework knows.

use plist::Value;

/// Read an integer plist field whether it was encoded signed or unsigned.
pub(super) fn int_field(v: &Value) -> Option<u64> {
    v.as_unsigned_integer()
        .or_else(|| v.as_signed_integer().map(|s| s as u64))
}
