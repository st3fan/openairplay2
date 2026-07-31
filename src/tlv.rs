//! HomeKit TLV8 encoding: a sequence of `[type:u8][len:u8][value:len]` items.
//! Values longer than 255 bytes are split into consecutive items of the same
//! type (each ≤255), reassembled by concatenation on decode.

/// HAP TLV item types used in pairing.
pub mod ty {
    pub const METHOD: u8 = 0x00;
    pub const IDENTIFIER: u8 = 0x01;
    pub const SALT: u8 = 0x02;
    pub const PUBLIC_KEY: u8 = 0x03;
    pub const PROOF: u8 = 0x04;
    pub const ENCRYPTED_DATA: u8 = 0x05;
    pub const STATE: u8 = 0x06;
    pub const ERROR: u8 = 0x07;
    pub const SIGNATURE: u8 = 0x0a;
    pub const FLAGS: u8 = 0x13;
}

/// A decoded TLV map: type → concatenated value bytes.
#[derive(Debug, Default)]
pub struct Tlv(Vec<(u8, Vec<u8>)>);

impl Tlv {
    pub fn new() -> Tlv {
        Tlv(Vec::new())
    }

    /// Append an item (fragmented into ≤255-byte chunks on encode).
    pub fn put(&mut self, ty: u8, value: impl Into<Vec<u8>>) -> &mut Tlv {
        self.0.push((ty, value.into()));
        self
    }

    /// Append a single-byte item (State, Method, Error, Flags).
    pub fn put_u8(&mut self, ty: u8, value: u8) -> &mut Tlv {
        self.put(ty, vec![value])
    }

    pub fn get(&self, ty: u8) -> Option<&[u8]> {
        self.0
            .iter()
            .find(|(t, _)| *t == ty)
            .map(|(_, v)| v.as_slice())
    }

    /// A single-byte item as `u8` (first byte).
    pub fn get_u8(&self, ty: u8) -> Option<u8> {
        self.get(ty).and_then(|v| v.first().copied())
    }

    /// Serialize to the TLV8 wire format, fragmenting long values.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for (ty, value) in &self.0 {
            if value.is_empty() {
                out.push(*ty);
                out.push(0);
                continue;
            }
            for chunk in value.chunks(255) {
                out.push(*ty);
                out.push(chunk.len() as u8);
                out.extend_from_slice(chunk);
            }
        }
        out
    }

    /// Parse the TLV8 wire format, merging consecutive same-type fragments.
    pub fn decode(bytes: &[u8]) -> Option<Tlv> {
        let mut items: Vec<(u8, Vec<u8>)> = Vec::new();
        let mut i = 0;
        while i < bytes.len() {
            let ty = bytes[i];
            let len = *bytes.get(i + 1)? as usize;
            let value = bytes.get(i + 2..i + 2 + len)?;
            i += 2 + len;
            // A fragment continues the previous item iff it has the same type
            // and that previous item was a full 255-byte fragment.
            match items.last_mut() {
                Some((last_ty, last_val))
                    if *last_ty == ty && last_val.len() % 255 == 0 && !last_val.is_empty() =>
                {
                    last_val.extend_from_slice(value);
                }
                _ => items.push((ty, value.to_vec())),
            }
        }
        Some(Tlv(items))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_simple_items() {
        let mut tlv = Tlv::new();
        tlv.put_u8(ty::STATE, 2).put(ty::SALT, vec![0xaa; 16]);
        let decoded = Tlv::decode(&tlv.encode()).unwrap();
        assert_eq!(decoded.get_u8(ty::STATE), Some(2));
        assert_eq!(decoded.get(ty::SALT), Some(&[0xaa; 16][..]));
        assert_eq!(decoded.get(ty::PROOF), None);
    }

    #[test]
    fn fragments_and_reassembles_long_values() {
        // 600 bytes → three fragments (255 + 255 + 90).
        let value: Vec<u8> = (0..600).map(|i| i as u8).collect();
        let mut tlv = Tlv::new();
        tlv.put(ty::PUBLIC_KEY, value.clone());
        let encoded = tlv.encode();
        // First fragment header at 0, second at 2+255, third at 2+255+2+255.
        assert_eq!(encoded[0], ty::PUBLIC_KEY);
        assert_eq!(encoded[1], 255);
        assert_eq!(encoded[257], ty::PUBLIC_KEY);
        assert_eq!(encoded[258], 255);
        let decoded = Tlv::decode(&encoded).unwrap();
        assert_eq!(decoded.get(ty::PUBLIC_KEY), Some(value.as_slice()));
    }

    #[test]
    fn exactly_255_then_more_reassembles() {
        // A 255-byte value followed by a genuinely separate same-type item is
        // ambiguous in TLV8; HAP resolves it by treating any run of same-type
        // items where each prior chunk is 255 as one value. Verify a 255-byte
        // value round-trips as itself.
        let value = vec![7u8; 255];
        let mut tlv = Tlv::new();
        tlv.put(ty::PUBLIC_KEY, value.clone());
        let decoded = Tlv::decode(&tlv.encode()).unwrap();
        assert_eq!(decoded.get(ty::PUBLIC_KEY), Some(value.as_slice()));
    }

    #[test]
    fn decode_rejects_truncated() {
        assert!(Tlv::decode(&[ty::SALT, 5, 1, 2]).is_none());
    }
}
