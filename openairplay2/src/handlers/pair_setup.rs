//! `POST /pair-setup` — the transient SRP exchange (M1→M2, M3→M4; see
//! [`crate::pairing`]). Every outcome, including a failed one, is a 200
//! carrying a TLV — errors travel *inside* the TLV in this protocol.

use crate::http::{Request, Response};
use crate::pairing::{Outcome, PairSetup};

use super::PAIRING_CONTENT_TYPE;

/// The response, plus — when pairing just completed — the SRP shared secret
/// the channel keys are derived from. Installing the cipher is a
/// connection-level act (it must happen right after the plaintext M4
/// response is written), so it stays with the caller in
/// [`crate::server`].
pub fn handle_pair_setup(request: &Request, pair: &mut PairSetup) -> (Response, Option<[u8; 64]>) {
    let (tlv, secret) = match pair.handle(&request.body) {
        Outcome::Continue(tlv) => (tlv, None),
        Outcome::Failed(tlv) => (tlv, None),
        Outcome::Done {
            response,
            shared_secret,
        } => (response, Some(shared_secret)),
    };
    (
        Response::ok(&request.protocol).body(PAIRING_CONTENT_TYPE, tlv),
        secret,
    )
}
