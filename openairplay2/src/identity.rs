//! Device identity: the Ed25519 keypair (`pk`) and the `pi` UUID.
//!
//! AirPlay 2 senders remember a receiver by its public key and public
//! identifier, so both must be stable across restarts. We persist them to a
//! small text file, generating them on first run.

use std::fs;
use std::io;
use std::path::Path;

use ed25519_dalek::SigningKey;
use rand::rngs::OsRng;
use uuid::Uuid;

pub struct Identity {
    signing_key: SigningKey,
    pi: Uuid,
}

impl Identity {
    /// Load the identity from `path`, or generate and persist a new one if the
    /// file doesn't exist.
    pub fn load_or_create(path: &Path) -> io::Result<Identity> {
        match fs::read_to_string(path) {
            Ok(contents) => Self::parse(&contents).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "malformed identity file")
            }),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let identity = Identity::generate();
                if let Some(parent) = path.parent() {
                    if !parent.as_os_str().is_empty() {
                        fs::create_dir_all(parent)?;
                    }
                }
                fs::write(path, identity.serialize())?;
                Ok(identity)
            }
            Err(e) => Err(e),
        }
    }

    pub fn generate() -> Identity {
        Identity {
            signing_key: SigningKey::generate(&mut OsRng),
            pi: Uuid::new_v4(),
        }
    }

    /// The 32-byte Ed25519 public key.
    pub fn public_key(&self) -> [u8; 32] {
        self.signing_key.verifying_key().to_bytes()
    }

    /// The public key as lowercase hex — the mDNS `pk` and `/info` `pk`.
    pub fn public_key_hex(&self) -> String {
        hex::encode(self.public_key())
    }

    /// The public identifier UUID (`pi`), lowercase, hyphenated.
    pub fn pi(&self) -> String {
        self.pi.to_string()
    }

    /// `<pk-hex>\n<pi>\n` — the persisted form (32-byte signing key seed as
    /// hex, then the UUID).
    fn serialize(&self) -> String {
        format!(
            "{}\n{}\n",
            hex::encode(self.signing_key.to_bytes()),
            self.pi
        )
    }

    fn parse(contents: &str) -> Option<Identity> {
        let mut lines = contents.lines();
        let seed = hex::decode(lines.next()?.trim()).ok()?;
        let seed: [u8; 32] = seed.try_into().ok()?;
        let pi = Uuid::parse_str(lines.next()?.trim()).ok()?;
        Some(Identity {
            signing_key: SigningKey::from_bytes(&seed),
            pi,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_serialize() {
        let id = Identity::generate();
        let restored = Identity::parse(&id.serialize()).unwrap();
        assert_eq!(restored.public_key(), id.public_key());
        assert_eq!(restored.pi(), id.pi());
    }

    #[test]
    fn public_key_hex_is_64_chars() {
        let id = Identity::generate();
        assert_eq!(id.public_key_hex().len(), 64);
    }

    #[test]
    fn load_or_create_persists_and_reloads() {
        let dir = std::env::temp_dir().join(format!("oap2-id-{}", std::process::id()));
        let path = dir.join("identity");
        let _ = fs::remove_dir_all(&dir);

        let first = Identity::load_or_create(&path).unwrap();
        let second = Identity::load_or_create(&path).unwrap();
        assert_eq!(
            first.public_key(),
            second.public_key(),
            "key must be stable"
        );
        assert_eq!(first.pi(), second.pi(), "pi must be stable");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn rejects_malformed_file() {
        assert!(Identity::parse("not hex\nnot a uuid\n").is_none());
    }
}
