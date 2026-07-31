//! The AirPlay `_airplay._tcp` TXT records and the `GET /info` response plist.

use plist::{Dictionary, Value};

use crate::identity::Identity;
use crate::Config;

/// The `_airplay._tcp` TXT records, matching what shairport-sync advertises in
/// AirPlay 2 mode. `gid` equals `pi` when the device is not in a group.
pub fn txt_records(config: &Config, identity: &Identity) -> Vec<String> {
    let (lo, hi) = config.features_split();
    vec![
        "acl=0".to_string(),
        format!("deviceid={}", config.device_id()),
        format!("features=0x{lo:X},0x{hi:X}"),
        format!("flags=0x{:X}", config.status_flags),
        format!("gid={}", identity.pi()),
        "gcgl=0".to_string(),
        "igl=0".to_string(),
        format!("model={}", config.model),
        "protovers=1.1".to_string(),
        format!("pi={}", identity.pi()),
        format!("pk={}", identity.public_key_hex()),
        format!("srcvers={}", config.source_version),
        "vv=2".to_string(),
    ]
}

/// Pack TXT records the way `txtAirPlay` wants them: each record as a
/// length-prefixed byte string (1-byte length + bytes), concatenated — the
/// DNS-SD TXT wire format.
pub fn pack_txt(records: &[String]) -> Vec<u8> {
    let mut out = Vec::new();
    for record in records {
        let bytes = record.as_bytes();
        out.push(bytes.len().min(255) as u8);
        out.extend_from_slice(&bytes[..bytes.len().min(255)]);
    }
    out
}

/// Build the binary plist returned by `GET /info`.
pub fn info_plist(config: &Config, identity: &Identity) -> Vec<u8> {
    let mut dict = Dictionary::new();

    // Static capability fields (from shairport-sync's get_info_response.xml).
    dict.insert("vv".into(), Value::Integer(2.into()));
    dict.insert("protocolVersion".into(), Value::String("1.1".into()));
    dict.insert("volumeControlType".into(), Value::Integer(3.into()));
    dict.insert("canRecordScreenStream".into(), Value::Boolean(false));
    dict.insert("keepAliveSendStatsAsBody".into(), Value::Boolean(false));
    dict.insert("screenDemoMode".into(), Value::Boolean(false));
    dict.insert("receiverHDRCapability".into(), Value::String("4k60".into()));
    let mut playback = Dictionary::new();
    playback.insert("supportsInterstitials".into(), Value::Boolean(false));
    playback.insert("supportsFPSSecureStop".into(), Value::Boolean(false));
    playback.insert(
        "supportsUIForAudioOnlyContent".into(),
        Value::Boolean(false),
    );
    dict.insert("playbackCapabilities".into(), Value::Dictionary(playback));

    // Device-specific fields.
    dict.insert("deviceID".into(), Value::String(config.device_id()));
    dict.insert("name".into(), Value::String(config.name.clone()));
    dict.insert("model".into(), Value::String(config.model.clone()));
    dict.insert(
        "sourceVersion".into(),
        Value::String(config.source_version.clone()),
    );
    dict.insert("features".into(), Value::Integer(config.features.into()));
    dict.insert(
        "statusFlags".into(),
        Value::Integer(u64::from(config.status_flags).into()),
    );
    dict.insert("pi".into(), Value::String(identity.pi()));
    dict.insert("pk".into(), Value::Data(identity.public_key().to_vec()));
    dict.insert(
        "txtAirPlay".into(),
        Value::Data(pack_txt(&txt_records(config, identity))),
    );

    let mut buf = Vec::new();
    Value::Dictionary(dict)
        .to_writer_binary(&mut buf)
        .expect("in-memory binary plist serialization cannot fail");
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (Config, Identity) {
        let config = Config {
            name: "Test Room".into(),
            port: 7000,
            mac: [0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff],
            model: "OpenAirPlay2,1".into(),
            source_version: "366.0".into(),
            features: 0x0001_8340_405C_4A00,
            status_flags: 0x4,
            alsa_device: None,
        };
        (config, Identity::generate())
    }

    #[test]
    fn txt_records_include_key_fields() {
        let (config, id) = fixture();
        let records = txt_records(&config, &id);
        assert!(records.iter().any(|r| r == "deviceid=AA:BB:CC:DD:EE:FF"));
        assert!(records.iter().any(|r| r == "features=0x405C4A00,0x18340"));
        assert!(records
            .iter()
            .any(|r| r == &format!("pk={}", id.public_key_hex())));
        assert!(records.iter().any(|r| r.starts_with("pi=")));
    }

    #[test]
    fn pack_txt_is_length_prefixed() {
        let packed = pack_txt(&["ab".to_string(), "cde".to_string()]);
        assert_eq!(packed, [2, b'a', b'b', 3, b'c', b'd', b'e']);
    }

    #[test]
    fn info_plist_parses_and_has_expected_fields() {
        let (config, id) = fixture();
        let bytes = info_plist(&config, &id);
        let value = plist::Value::from_reader(std::io::Cursor::new(bytes)).unwrap();
        let dict = value.as_dictionary().unwrap();

        assert_eq!(
            dict.get("deviceID").unwrap().as_string(),
            Some("AA:BB:CC:DD:EE:FF")
        );
        assert_eq!(
            dict.get("protocolVersion").unwrap().as_string(),
            Some("1.1")
        );
        assert_eq!(
            dict.get("features").unwrap().as_unsigned_integer(),
            Some(0x0001_8340_405C_4A00)
        );
        assert_eq!(
            dict.get("pk").unwrap().as_data().unwrap(),
            &id.public_key()[..]
        );
        assert!(dict.get("txtAirPlay").unwrap().as_data().is_some());
    }
}
