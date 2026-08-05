//! `pair-setup` transient state machine (the receiver / server side).
//!
//! Transient pairing (the sender sets the transient flag, fixed code `3939`)
//! is a two-round SRP exchange — M1→M2, M3→M4 — after which the SRP session
//! key `K` is the shared secret for the encrypted channel. Persistent
//! (non-transient) pairing, which additionally exchanges Ed25519 identities in
//! M5/M6, is not implemented here.

use log::{debug, warn};

use crate::srp::SrpServer;
use crate::tlv::{ty, Tlv};

const STATE_M2: u8 = 2;
const STATE_M4: u8 = 4;
const FLAG_TRANSIENT: u8 = 0x10;
const ERROR_AUTHENTICATION: u8 = 0x02;
const PAIR_SETUP_PIN: &str = "3939";

pub enum Outcome {
    /// A TLV response; the exchange continues.
    Continue(Vec<u8>),
    /// Transient pairing completed; install the cipher with this shared secret
    /// after sending the response.
    Done {
        response: Vec<u8>,
        shared_secret: [u8; 64],
    },
    /// A TLV error response; the exchange failed.
    Failed(Vec<u8>),
}

#[derive(Default)]
pub struct PairSetup {
    srp: Option<SrpServer>,
    transient: bool,
    /// The SRP password: `3939` for transient, or the configured pincode.
    pin: String,
}

impl PairSetup {
    pub fn new(pincode: Option<&str>) -> PairSetup {
        PairSetup {
            srp: None,
            transient: false,
            pin: pincode.unwrap_or(PAIR_SETUP_PIN).to_string(),
        }
    }

    /// Process one `POST /pair-setup` request body (TLV8).
    pub fn handle(&mut self, body: &[u8]) -> Outcome {
        let Some(request) = Tlv::decode(body) else {
            return failed(STATE_M2, "malformed TLV");
        };
        match request.get_u8(ty::STATE) {
            Some(1) => self.handle_m1(&request),
            Some(3) => self.handle_m3(&request),
            other => failed(STATE_M2, &format!("unexpected pair-setup state {other:?}")),
        }
    }

    /// M1 → M2: start SRP, reply with salt and the server public key `B`.
    fn handle_m1(&mut self, request: &Tlv) -> Outcome {
        if request.get_u8(ty::METHOD) != Some(0) {
            return failed(STATE_M2, "pair-setup method must be 0");
        }
        self.transient = request
            .get(ty::FLAGS)
            .and_then(|f| f.first())
            .is_some_and(|b| b & FLAG_TRANSIENT != 0);
        debug!("pair-setup M1 (transient={})", self.transient);

        let srp = SrpServer::new(&self.pin);
        let mut response = Tlv::new();
        response
            .put_u8(ty::STATE, STATE_M2)
            .put(ty::SALT, srp.salt().to_vec())
            .put(ty::PUBLIC_KEY, srp.public_b());
        self.srp = Some(srp);
        Outcome::Continue(response.encode())
    }

    /// M3 → M4: verify the client proof, reply with the server proof `HAMK`.
    /// On transient success the shared secret is the SRP session key.
    fn handle_m3(&mut self, request: &Tlv) -> Outcome {
        let Some(srp) = self.srp.as_mut() else {
            return failed(STATE_M4, "M3 before M1");
        };
        let (Some(a), Some(m1)) = (request.get(ty::PUBLIC_KEY), request.get(ty::PROOF)) else {
            return failed(STATE_M4, "M3 missing public key or proof");
        };

        let Some(hamk) = srp.verify(a, m1) else {
            warn!("pair-setup M3: SRP proof verification failed");
            return failed(STATE_M4, "SRP authentication failed");
        };

        let mut response = Tlv::new();
        response
            .put_u8(ty::STATE, STATE_M4)
            .put(ty::PROOF, hamk.to_vec());
        let response = response.encode();

        if self.transient {
            let shared_secret = *srp.session_key().expect("session key set after verify");
            debug!("pair-setup complete (transient); channel now encrypted");
            Outcome::Done {
                response,
                shared_secret,
            }
        } else {
            // Persistent pairing would continue with M5/M6 identity exchange,
            // which we don't implement. Answer M4 so the sender sees progress.
            warn!("non-transient pair-setup is not supported; expecting transient");
            Outcome::Continue(response)
        }
    }
}

fn failed(state: u8, reason: &str) -> Outcome {
    warn!("pair-setup failed at M{state}: {reason}");
    let mut tlv = Tlv::new();
    tlv.put_u8(ty::STATE, state)
        .put_u8(ty::ERROR, ERROR_AUTHENTICATION);
    Outcome::Failed(tlv.encode())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::srp::SrpClient;

    /// Drive a full transient pair-setup as a client and return the agreed
    /// shared secret from both sides.
    #[test]
    fn transient_pair_setup_completes_with_matching_secret() {
        let mut setup = PairSetup::new(None);

        // M1: client starts, transient flag set.
        let mut m1 = Tlv::new();
        m1.put_u8(ty::STATE, 1)
            .put_u8(ty::METHOD, 0)
            .put_u8(ty::FLAGS, 0x10);
        let m2 = match setup.handle(&m1.encode()) {
            Outcome::Continue(r) => Tlv::decode(&r).unwrap(),
            _ => panic!("expected M2"),
        };
        assert_eq!(m2.get_u8(ty::STATE), Some(2));

        // Client computes its proof from the salt and B.
        let mut client = SrpClient::new("3939");
        let m1_proof = client.process(m2.get(ty::SALT).unwrap(), m2.get(ty::PUBLIC_KEY).unwrap());

        // M3: client sends A + proof.
        let mut m3 = Tlv::new();
        m3.put_u8(ty::STATE, 3)
            .put(ty::PUBLIC_KEY, client.public_a())
            .put(ty::PROOF, m1_proof.to_vec());
        match setup.handle(&m3.encode()) {
            Outcome::Done {
                response,
                shared_secret,
            } => {
                let m4 = Tlv::decode(&response).unwrap();
                assert_eq!(m4.get_u8(ty::STATE), Some(4));
                assert!(client.verify_hamk(m4.get(ty::PROOF).unwrap(), &m1_proof));
                assert_eq!(&shared_secret, client.session_key().unwrap());
            }
            _ => panic!("expected transient Done"),
        }
    }

    #[test]
    fn wrong_pin_fails_at_m3() {
        let mut setup = PairSetup::new(None);
        let mut m1 = Tlv::new();
        m1.put_u8(ty::STATE, 1)
            .put_u8(ty::METHOD, 0)
            .put_u8(ty::FLAGS, 0x10);
        let m2 = match setup.handle(&m1.encode()) {
            Outcome::Continue(r) => Tlv::decode(&r).unwrap(),
            _ => panic!("expected M2"),
        };
        let mut client = SrpClient::new("9999"); // wrong
        let proof = client.process(m2.get(ty::SALT).unwrap(), m2.get(ty::PUBLIC_KEY).unwrap());
        let mut m3 = Tlv::new();
        m3.put_u8(ty::STATE, 3)
            .put(ty::PUBLIC_KEY, client.public_a())
            .put(ty::PROOF, proof.to_vec());
        match setup.handle(&m3.encode()) {
            Outcome::Failed(r) => {
                let tlv = Tlv::decode(&r).unwrap();
                assert_eq!(tlv.get_u8(ty::ERROR), Some(ERROR_AUTHENTICATION));
            }
            _ => panic!("expected failure"),
        }
    }

    /// Drive M1→M3 and return the outcome, as a client presenting `pin`.
    fn run_setup(setup: &mut PairSetup, pin: &str) -> Outcome {
        let mut m1 = Tlv::new();
        m1.put_u8(ty::STATE, 1)
            .put_u8(ty::METHOD, 0)
            .put_u8(ty::FLAGS, 0x10);
        let m2 = match setup.handle(&m1.encode()) {
            Outcome::Continue(r) => Tlv::decode(&r).unwrap(),
            other => return other,
        };
        let mut client = SrpClient::new(pin);
        let proof = client.process(m2.get(ty::SALT).unwrap(), m2.get(ty::PUBLIC_KEY).unwrap());
        let mut m3 = Tlv::new();
        m3.put_u8(ty::STATE, 3)
            .put(ty::PUBLIC_KEY, client.public_a())
            .put(ty::PROOF, proof.to_vec());
        setup.handle(&m3.encode())
    }

    /// A configured pincode becomes the SRP password: a client that presents
    /// it pairs; the standard `3939` is refused.
    #[test]
    fn configured_pincode_is_the_srp_password() {
        let mut setup = PairSetup::new(Some("1212"));
        assert!(matches!(
            run_setup(&mut setup, "1212"),
            Outcome::Done { .. }
        ));

        let mut setup = PairSetup::new(Some("1212"));
        assert!(matches!(run_setup(&mut setup, "3939"), Outcome::Failed(_)));

        // No pincode configured: the standard transient 3939 still pairs.
        let mut setup = PairSetup::new(None);
        assert!(matches!(
            run_setup(&mut setup, "3939"),
            Outcome::Done { .. }
        ));
    }
}
