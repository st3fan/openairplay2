//! X25519 (Curve25519) ephemeral key agreement, used by HomeKit `pair-verify`.
//!
//! `pair-verify` (the ongoing authentication after persistent pairing) agrees
//! a shared secret over X25519 between each side's ephemeral keys, then signs
//! and HKDFs the result. This module is the unambiguous primitive only — the
//! wire framing and HKDF info strings are pinned against a real sender in
//! phase 2 (`notes/protocol.md`) before the full state machine is built.
#![allow(dead_code)] // groundwork: wired into pair-verify in phase 4

use rand::rngs::OsRng;
use x25519_dalek::{PublicKey, StaticSecret};

/// A freshly-generated X25519 ephemeral keypair (secret + public).
pub struct Ephemeral {
    secret: StaticSecret,
    public: PublicKey,
}

impl Ephemeral {
    /// Generate a fresh, unpredictable ephemeral keypair.
    pub fn generate() -> Ephemeral {
        let secret = StaticSecret::random_from_rng(OsRng);
        let public = PublicKey::from(&secret);
        Ephemeral { secret, public }
    }

    /// The 32-byte X25519 public key, to send to the peer.
    pub fn public_bytes(&self) -> [u8; 32] {
        self.public.to_bytes()
    }

    /// The X25519 shared secret with a peer's public key. Commutative: both
    /// sides calling this with the other's public key derive the same bytes
    /// (the base-point clamped scalar multiplication).
    pub fn shared_with(&self, peer_public: &[u8; 32]) -> [u8; 32] {
        *self
            .secret
            .diffie_hellman(&PublicKey::from(*peer_public))
            .as_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_sides_derive_the_same_secret() {
        let a = Ephemeral::generate();
        let b = Ephemeral::generate();
        assert_eq!(
            a.shared_with(&b.public_bytes()),
            b.shared_with(&a.public_bytes()),
            "X25519 shared secret must be commutative"
        );
    }

    #[test]
    fn different_peer_derives_a_different_secret() {
        let a = Ephemeral::generate();
        let b = Ephemeral::generate();
        let c = Ephemeral::generate();
        assert_ne!(
            a.shared_with(&b.public_bytes()),
            a.shared_with(&c.public_bytes()),
            "two different peers must not derive the same secret"
        );
        // And it's the 32-byte key the protocol expects.
        assert_eq!(Ephemeral::generate().public_bytes().len(), 32);
    }
}
