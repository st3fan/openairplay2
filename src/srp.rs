//! SRP-6a, the exact HomeKit variant used by AirPlay 2 `pair-setup`:
//! RFC 5054 3072-bit group, `g = 5`, SHA-512, username `Pair-Setup`.
//!
//! Non-standard details that must match Apple (verified against
//! shairport-sync's csrp fork):
//! - `k = H(PAD(N) ‖ PAD(g))`, `u = H(PAD(A) ‖ PAD(B))` padded to the modulus
//!   length,
//! - session key `K = H(S)` — the *hash* of S,
//! - `M1 = H((H(N)⊕H(g)) ‖ H(I) ‖ s ‖ A ‖ B ‖ K)`, `HAMK = H(A ‖ M1 ‖ K)`,
//!   with s/A/B/N as minimal big-endian bytes.

use num_bigint::BigUint;
use rand::RngCore;
use sha2::{Digest, Sha512};

pub const USERNAME: &str = "Pair-Setup";
const N_LEN: usize = 384;

// RFC 5054 3072-bit group prime, big-endian hex.
const N_HEX: &str = "\
FFFFFFFFFFFFFFFFC90FDAA22168C234C4C6628B80DC1CD129024E088A67CC74020BBEA63B\
139B22514A08798E3404DDEF9519B3CD3A431B302B0A6DF25F14374FE1356D6D51C245E485\
B576625E7EC6F44C42E9A637ED6B0BFF5CB6F406B7EDEE386BFB5A899FA5AE9F24117C4B1F\
E649286651ECE45B3DC2007CB8A163BF0598DA48361C55D39A69163FA8FD24CF5F83655D23\
DCA3AD961C62F356208552BB9ED529077096966D670C354E4ABC9804F1746C08CA18217C32\
905E462E36CE3BE39E772C180E86039B2783A2EC07A28FB5C55DF06F4C52C9DE2BCBF69558\
17183995497CEA956AE515D2261898FA051015728E5A8AAAC42DAD33170D04507A33A85521\
ABDF1CBA64ECFB850458DBEF0A8AEA71575D060C7DB3970F85A6E1E4C7ABF5AE8CDB0933D7\
1E8C94E04A25619DCEE3D2261AD2EE6BF12FFA06D98A0864D87602733EC86A64521F2B1817\
7B200CBBE117577A615D6C770988C0BAD946E208E24FA074E5AB3143DB5BFCE0FD108E4B82\
D120A93AD2CAFFFFFFFFFFFFFFFF";

fn group() -> (BigUint, BigUint) {
    let n = BigUint::parse_bytes(N_HEX.as_bytes(), 16).expect("valid N");
    (n, BigUint::from(5u32))
}

fn h(parts: &[&[u8]]) -> [u8; 64] {
    let mut hasher = Sha512::new();
    for p in parts {
        hasher.update(p);
    }
    hasher.finalize().into()
}

/// Left-pad a number's big-endian bytes to `N_LEN`.
fn pad(n: &BigUint) -> Vec<u8> {
    let bytes = n.to_bytes_be();
    let mut out = vec![0u8; N_LEN.saturating_sub(bytes.len())];
    out.extend_from_slice(&bytes);
    out
}

/// `k = H(PAD(N) ‖ PAD(g))`.
fn compute_k(n: &BigUint, g: &BigUint) -> BigUint {
    BigUint::from_bytes_be(&h(&[&pad(n), &pad(g)]))
}

/// `u = H(PAD(A) ‖ PAD(B))`.
fn compute_u(a: &BigUint, b: &BigUint) -> BigUint {
    BigUint::from_bytes_be(&h(&[&pad(a), &pad(b)]))
}

/// `x = H(s ‖ H(I ":" p))` — the private key derived from salt and password.
fn compute_x(salt: &[u8], username: &str, password: &str) -> BigUint {
    let inner = h(&[username.as_bytes(), b":", password.as_bytes()]);
    BigUint::from_bytes_be(&h(&[salt, &inner]))
}

/// `M1 = H((H(N)⊕H(g)) ‖ H(I) ‖ s ‖ A ‖ B ‖ K)` (minimal big-endian bytes).
fn compute_m1(
    n: &BigUint,
    g: &BigUint,
    username: &str,
    salt: &[u8],
    a: &BigUint,
    b: &BigUint,
    k_session: &[u8],
) -> [u8; 64] {
    let h_n = h(&[&n.to_bytes_be()]);
    let h_g = h(&[&g.to_bytes_be()]);
    let mut h_xor = [0u8; 64];
    for i in 0..64 {
        h_xor[i] = h_n[i] ^ h_g[i];
    }
    let h_i = h(&[username.as_bytes()]);
    h(&[
        &h_xor,
        &h_i,
        salt,
        &a.to_bytes_be(),
        &b.to_bytes_be(),
        k_session,
    ])
}

/// `HAMK = H(A ‖ M1 ‖ K)` — the server's proof.
fn compute_hamk(a: &BigUint, m1: &[u8], k_session: &[u8]) -> [u8; 64] {
    h(&[&a.to_bytes_be(), m1, k_session])
}

fn random_exponent() -> BigUint {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    BigUint::from_bytes_be(&bytes)
}

/// SRP-6a server (verifier) side, driving `pair-setup` for a receiver.
pub struct SrpServer {
    n: BigUint,
    g: BigUint,
    salt: [u8; 16],
    v: BigUint,
    b: BigUint,
    big_b: BigUint,
    session_key: Option<[u8; 64]>,
}

impl SrpServer {
    pub fn new(password: &str) -> SrpServer {
        let (n, g) = group();
        let mut salt = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut salt);

        let x = compute_x(&salt, USERNAME, password);
        let v = g.modpow(&x, &n);
        let k = compute_k(&n, &g);
        let b = random_exponent();
        // B = (k*v + g^b) mod N
        let big_b = (&k * &v + g.modpow(&b, &n)) % &n;

        SrpServer {
            n,
            g,
            salt,
            v,
            b,
            big_b,
            session_key: None,
        }
    }

    pub fn salt(&self) -> &[u8; 16] {
        &self.salt
    }

    /// The server public key `B`, minimal big-endian.
    pub fn public_b(&self) -> Vec<u8> {
        self.big_b.to_bytes_be()
    }

    /// Verify the client's proof `M1` over its public key `A`. On success,
    /// stores the session key and returns the server proof `HAMK`.
    pub fn verify(&mut self, a_bytes: &[u8], m1: &[u8]) -> Option<[u8; 64]> {
        let a = BigUint::from_bytes_be(a_bytes);
        if &a % &self.n == BigUint::from(0u32) {
            return None; // SRP-6a safety check
        }
        let u = compute_u(&a, &self.big_b);
        // S = (A * v^u)^b mod N
        let s = (&a * self.v.modpow(&u, &self.n)).modpow(&self.b, &self.n);
        let k_session = h(&[&s.to_bytes_be()]);

        let expected = compute_m1(
            &self.n,
            &self.g,
            USERNAME,
            &self.salt,
            &a,
            &self.big_b,
            &k_session,
        );
        if !constant_time_eq(&expected, m1) {
            return None;
        }
        self.session_key = Some(k_session);
        Some(compute_hamk(&a, m1, &k_session))
    }

    /// The 64-byte SRP session key `K`, available after a successful `verify`.
    pub fn session_key(&self) -> Option<&[u8; 64]> {
        self.session_key.as_ref()
    }
}

/// SRP-6a client, used by tests and the synthetic sender to drive the server.
pub struct SrpClient {
    n: BigUint,
    g: BigUint,
    a: BigUint,
    big_a: BigUint,
    password: String,
    session_key: Option<[u8; 64]>,
}

impl SrpClient {
    pub fn new(password: &str) -> SrpClient {
        let (n, g) = group();
        let a = random_exponent();
        let big_a = g.modpow(&a, &n);
        SrpClient {
            n,
            g,
            a,
            big_a,
            password: password.to_string(),
            session_key: None,
        }
    }

    /// The client public key `A`, minimal big-endian.
    pub fn public_a(&self) -> Vec<u8> {
        self.big_a.to_bytes_be()
    }

    /// Given the server's salt and `B`, compute the client proof `M1` (and the
    /// session key, kept for `verify_hamk`).
    pub fn process(&mut self, salt: &[u8], b_bytes: &[u8]) -> [u8; 64] {
        let big_b = BigUint::from_bytes_be(b_bytes);
        let x = compute_x(salt, USERNAME, &self.password);
        let u = compute_u(&self.big_a, &big_b);
        let k = compute_k(&self.n, &self.g);
        // S = (B - k*g^x)^(a + u*x) mod N, with modular subtraction.
        let kgx = (&k * self.g.modpow(&x, &self.n)) % &self.n;
        let base = (&big_b + &self.n - kgx) % &self.n;
        let exp = &self.a + &u * &x;
        let s = base.modpow(&exp, &self.n);
        let k_session = h(&[&s.to_bytes_be()]);
        self.session_key = Some(k_session);
        compute_m1(
            &self.n,
            &self.g,
            USERNAME,
            salt,
            &self.big_a,
            &big_b,
            &k_session,
        )
    }

    pub fn verify_hamk(&self, hamk: &[u8], m1: &[u8]) -> bool {
        let k = self.session_key.expect("process() first");
        constant_time_eq(&compute_hamk(&self.big_a, m1, &k), hamk)
    }

    pub fn session_key(&self) -> Option<&[u8; 64]> {
        self.session_key.as_ref()
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_and_server_derive_the_same_key() {
        let mut server = SrpServer::new("3939");
        let mut client = SrpClient::new("3939");

        let m1 = client.process(server.salt(), &server.public_b());
        let hamk = server
            .verify(&client.public_a(), &m1)
            .expect("M1 must verify");
        assert!(client.verify_hamk(&hamk, &m1), "HAMK must verify");

        assert_eq!(
            server.session_key().unwrap(),
            client.session_key().unwrap(),
            "both sides must agree on K"
        );
    }

    #[test]
    fn wrong_password_fails_verification() {
        let mut server = SrpServer::new("3939");
        let mut client = SrpClient::new("0000");
        let m1 = client.process(server.salt(), &server.public_b());
        assert!(server.verify(&client.public_a(), &m1).is_none());
    }

    #[test]
    fn session_key_is_64_bytes() {
        let mut server = SrpServer::new("3939");
        let mut client = SrpClient::new("3939");
        let m1 = client.process(server.salt(), &server.public_b());
        server.verify(&client.public_a(), &m1).unwrap();
        assert_eq!(server.session_key().unwrap().len(), 64);
    }
}
