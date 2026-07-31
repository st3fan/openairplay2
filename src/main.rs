use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use log::{info, warn};
use tokio::net::TcpListener;

use openairplay2::identity::Identity;
use openairplay2::info::txt_records;
use openairplay2::server::{serve, Context};
use openairplay2::{avahi, mac, Config};

const DEFAULT_NAME: &str = "OpenAirPlay2";
const DEFAULT_PORT: u16 = 7000;
const DEFAULT_MODEL: &str = "OpenAirPlay2,1";
const DEFAULT_SOURCE_VERSION: &str = "366.0";
// shairport-sync's known-good AirPlay 2 features: transient pairing (bit 48)
// plus AirPlay 2 audio. Pared down later as needed.
const DEFAULT_FEATURES: u64 = 0x0001_8340_405C_4A00;
const DEFAULT_STATUS_FLAGS: u32 = 0x4;
const FALLBACK_MAC: [u8; 6] = [0x02, 0x4f, 0x41, 0x50, 0x32, 0x00];
const DEFAULT_ALSA_DEVICE: &str = "default";

struct Args {
    name: String,
    port: u16,
    mac: Option<[u8; 6]>,
    identity_file: Option<PathBuf>,
    avahi: bool,
    /// ALSA device, or `None` for `--no-audio`.
    alsa_device: Option<String>,
}

fn usage() -> ! {
    eprintln!(
        "usage: openairplay2 [--name NAME] [--port PORT] [--mac AA:BB:CC:DD:EE:FF] \
         [--identity-file PATH] [--no-avahi] [--alsa-device NAME] [--no-audio]"
    );
    std::process::exit(2);
}

fn parse_args() -> Args {
    let mut args = Args {
        name: DEFAULT_NAME.to_string(),
        port: DEFAULT_PORT,
        mac: None,
        identity_file: None,
        avahi: true,
        alsa_device: Some(DEFAULT_ALSA_DEVICE.to_string()),
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--name" => args.name = it.next().unwrap_or_else(|| usage()),
            "--port" => {
                args.port = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| usage())
            }
            "--mac" => {
                args.mac = Some(
                    it.next()
                        .as_deref()
                        .and_then(mac::parse)
                        .unwrap_or_else(|| usage()),
                )
            }
            "--identity-file" => {
                args.identity_file = Some(PathBuf::from(it.next().unwrap_or_else(|| usage())))
            }
            "--no-avahi" => args.avahi = false,
            "--alsa-device" => args.alsa_device = Some(it.next().unwrap_or_else(|| usage())),
            "--no-audio" => args.alsa_device = None,
            "-h" | "--help" => usage(),
            other => {
                eprintln!("unknown argument: {other}");
                usage();
            }
        }
    }
    args
}

fn default_identity_path() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(".config/openairplay2/identity"),
        None => PathBuf::from("openairplay2.identity"),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = parse_args();

    let mac = args.mac.or_else(mac::discover).unwrap_or_else(|| {
        warn!("no network interface MAC found, using a fixed fallback");
        FALLBACK_MAC
    });

    let identity_path = args.identity_file.unwrap_or_else(default_identity_path);
    let identity = match Identity::load_or_create(&identity_path) {
        Ok(id) => id,
        Err(e) => {
            eprintln!("cannot load or create identity at {identity_path:?}: {e}");
            return ExitCode::FAILURE;
        }
    };

    let config = Config {
        name: args.name,
        port: args.port,
        mac,
        model: DEFAULT_MODEL.to_string(),
        source_version: DEFAULT_SOURCE_VERSION.to_string(),
        features: DEFAULT_FEATURES,
        status_flags: DEFAULT_STATUS_FLAGS,
        alsa_device: args.alsa_device,
    };
    info!(
        "starting AirPlay 2 receiver \"{}\" (deviceid {}, port {}, pk {})",
        config.name,
        config.device_id(),
        config.port,
        identity.public_key_hex()
    );
    match &config.alsa_device {
        Some(dev) => info!("audio output: ALSA \"{dev}\""),
        None => info!("audio output: disabled (--no-audio)"),
    }

    // Dual-stack if possible (IPv4 clients arrive v4-mapped), else IPv4.
    let listener = match TcpListener::bind(SocketAddr::from((Ipv6Addr::UNSPECIFIED, config.port)))
        .await
    {
        Ok(l) => l,
        Err(_) => {
            match TcpListener::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, config.port))).await {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("cannot bind control port {}: {e}", config.port);
                    return ExitCode::FAILURE;
                }
            }
        }
    };

    let _advertisement = if args.avahi {
        let records = txt_records(&config, &identity);
        match avahi::publish(&config.name, config.port, &records).await {
            Ok(ad) => Some(ad),
            Err(e) => {
                warn!("avahi advertisement disabled ({e}); is avahi-daemon running?");
                None
            }
        }
    } else {
        None
    };

    let context = Arc::new(Context { config, identity });
    tokio::select! {
        result = serve(listener, context) => {
            if let Err(e) = result {
                eprintln!("server error: {e}");
                return ExitCode::FAILURE;
            }
        }
        _ = tokio::signal::ctrl_c() => {
            info!("shutting down");
        }
    }
    ExitCode::SUCCESS
}
