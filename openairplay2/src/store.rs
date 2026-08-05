//! A durable registry of paired controllers.
//!
//! Persistent pairing records the controllers that have completed `pair-setup`
//! with this receiver; `pair-verify` on later connections looks a controller up
//! here (by its long-term Ed25519 public key) and only lets paired devices
//! stream. The registry is persisted so a pairing survives a restart.
//!
//! Format is a simple line-based text file (like `identity`), written
//! atomically: `identifier` tab `64-hex public key` per line. Phase 3 will add
//! the SRP salt/verifier per entry so a re-setup can re-run SRP; the file
//! format is versioned here so that addition stays compatible.
#![allow(dead_code)] // groundwork: wired into persistent pairing in phases 3-4

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use hex::FromHex;

/// One paired controller: its stable identifier and long-term Ed25519 key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pairing {
    /// The controller's stable identifier (e.g. its UUID).
    pub identifier: String,
    /// Its 32-byte Ed25519 public key, the lookup key for `pair-verify`.
    pub public_key: [u8; 32],
}

impl Pairing {
    /// A new pairing entry.
    pub fn new(identifier: impl Into<String>, public_key: [u8; 32]) -> Pairing {
        Pairing {
            identifier: identifier.into(),
            public_key,
        }
    }
}

/// The paired-controller registry. Not `Clone` by design — there is one
/// authoritative store behind the server.
pub struct Store {
    path: PathBuf,
    entries: Vec<Pairing>,
}

impl Store {
    /// Load the store from `path`, creating an empty one if the file is absent
    /// or empty. A malformed line is skipped (never a startup failure).
    pub fn load(path: impl Into<PathBuf>) -> io::Result<Store> {
        let path = path.into();
        let entries = match fs::read_to_string(&path) {
            Ok(text) => text.lines().filter_map(Self::parse_line).collect(),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            Err(e) => return Err(e),
        };
        Ok(Store { path, entries })
    }

    /// The path the store was loaded from / saves to.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether a controller with `public_key` is paired.
    pub fn contains(&self, public_key: &[u8; 32]) -> bool {
        self.entries.iter().any(|p| &p.public_key == public_key)
    }

    /// The pairing for `public_key`, if any.
    pub fn find(&self, public_key: &[u8; 32]) -> Option<&Pairing> {
        self.entries.iter().find(|p| &p.public_key == public_key)
    }

    /// The number of paired controllers.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Record a pairing, replacing any existing one with the same public key
    /// (a controller re-pairing updates its identifier). Persisted by
    /// [`Store::save`].
    pub fn insert(&mut self, pairing: Pairing) {
        let key = pairing.public_key;
        self.entries.retain(|p| p.public_key != key);
        self.entries.push(pairing);
    }

    /// Remove a controller by public key; returns whether one was removed.
    /// Persisted by [`Store::save`].
    pub fn remove(&mut self, public_key: &[u8; 32]) -> bool {
        let before = self.entries.len();
        self.entries.retain(|p| &p.public_key != public_key);
        before != self.entries.len()
    }

    /// Persist atomically: write a temp file alongside, then rename over the
    /// target, so a crash mid-write never leaves a half-written store.
    pub fn save(&self) -> io::Result<()> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir)?;
        }
        let mut text = String::new();
        for p in &self.entries {
            text.push_str(&p.identifier);
            text.push('\t');
            text.push_str(&hex::encode(p.public_key));
            text.push('\n');
        }
        let tmp = self.path.with_extension("tmp");
        fs::write(&tmp, text)?;
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    fn parse_line(line: &str) -> Option<Pairing> {
        let (identifier, key_hex) = line.trim().split_once('\t')?;
        if identifier.is_empty() {
            return None;
        }
        let key_bytes = <[u8; 32]>::from_hex(key_hex).ok()?;
        Some(Pairing::new(identifier.to_string(), key_bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_path(tag: &str) -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("oap2-store-{tag}-{n}.txt"))
    }

    fn key(seed: u8) -> [u8; 32] {
        [seed; 32]
    }

    #[test]
    fn round_trips_through_disk() {
        let path = tmp_path("roundtrip");
        let mut store = Store::load(&path).unwrap();
        store.insert(Pairing::new("aa-bb", key(1)));
        store.insert(Pairing::new("cc-dd", key(2)));
        store.save().unwrap();

        let reloaded = Store::load(&path).unwrap();
        assert_eq!(reloaded.len(), 2);
        assert!(reloaded.contains(&key(1)));
        assert!(reloaded.contains(&key(2)));
        assert_eq!(reloaded.find(&key(2)).unwrap().identifier, "cc-dd");
        let _ = fs::remove_file(&path).ok();
    }

    #[test]
    fn missing_or_empty_file_is_an_empty_store() {
        let path = tmp_path("empty");
        let store = Store::load(&path).unwrap();
        assert_eq!(store.len(), 0);
        let _ = fs::remove_file(&path).ok();
    }

    #[test]
    fn lookup_miss_returns_none() {
        let mut store = Store::load(tmp_path("miss")).unwrap();
        store.insert(Pairing::new("aa", key(7)));
        assert!(store.contains(&key(7)));
        assert!(!store.contains(&key(8)));
        assert!(store.find(&key(8)).is_none());
    }

    #[test]
    fn insert_replaces_and_remove_deletes() {
        let mut store = Store::load(tmp_path("replace")).unwrap();
        store.insert(Pairing::new("first", key(5)));
        store.insert(Pairing::new("second", key(5)));
        assert_eq!(store.len(), 1, "re-insert replaces by public key");
        assert_eq!(store.find(&key(5)).unwrap().identifier, "second");

        assert!(store.remove(&key(5)));
        assert!(!store.remove(&key(5)), "second remove finds nothing");
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let path = tmp_path("malformed");
        let valid_key: [u8; 32] = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff, 0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb,
            0xcc, 0xdd, 0xee, 0xff,
        ];
        fs::write(
            &path,
            format!(
                "ok-pair\t{}\nno-tab-here\nshortkey\t00ff00ff\n",
                hex::encode(valid_key)
            ),
        )
        .unwrap();
        let store = Store::load(&path).unwrap();
        assert_eq!(store.len(), 1, "only the valid line loads");
        assert!(store.contains(&valid_key), "the valid entry is present");
        let _ = fs::remove_file(&path).ok();
    }
}
