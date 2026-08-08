//! The standalone Linux/ALSA AirPlay 2 receiver: a CLI over the
//! `openairplay2` library's public API (it is embedder #1), with an ALSA
//! sink and the dB → linear gain volume model.

use std::path::PathBuf;
use std::process::ExitCode;

use log::{debug, info, warn};
use tokio::signal::unix::SignalKind;

mod player;
mod tui;

use crate::player::{volume_to_gain, AlsaSink, NullSink, SharedGain};
use openairplay2::{AudioSink, Event, Receiver};

const DEFAULT_ALSA_DEVICE: &str = "default";

#[derive(Debug, PartialEq)]
struct Args {
    /// `None` → the library's defaults (name "OpenAirPlay2", port 7000).
    name: Option<String>,
    port: Option<u16>,
    mac: Option<[u8; 6]>,
    identity_file: Option<PathBuf>,
    avahi: bool,
    /// ALSA device, or `None` for `--no-audio`.
    alsa_device: Option<String>,
    /// Require this pincode to pair; `None` → transient `3939`.
    pincode: Option<String>,
    /// Address to serve the now-playing WebSocket on; off when `None`.
    tui_listen: Option<String>,
}

/// What the command line asks for; everything but `Run` prints and exits.
#[derive(Debug, PartialEq)]
enum Action {
    Run(Args),
    Help,
    Version,
    ListDevices,
}

const USAGE: &str = "usage: openairplay2-receiver [options] — run with --help for the list";

const HELP: &str = "\
openairplay2-receiver — standalone AirPlay 2 audio receiver (ALSA)

usage: openairplay2-receiver [options]

options:
  --name NAME               name senders see in the AirPlay menu
                            (default \"OpenAirPlay2\")
  --port PORT               control port (default 7000)
  --mac AA:BB:CC:DD:EE:FF   device id in the advertisement (default: taken
                            from a real network interface)
  --identity-file PATH      where the pairing identity lives — senders
                            remember this receiver by it, so keep it stable
                            (default ~/.config/openairplay2/identity)
  --pincode CODE            require this code to pair; without it any device
                            on the network can pair
  --no-avahi                do not advertise over mDNS
  --alsa-device NAME        ALSA playback device (default \"default\");
                            see --list-devices for what this machine has
  --no-audio                decode but do not open ALSA (silent test run)
  --tui-listen ADDR         serve the now-playing WebSocket that
                            openairplay2-tui renders, e.g. 127.0.0.1:7392; it
                            carries track metadata and cover art, so keep it
                            on loopback unless you mean otherwise
  --list-devices            list the ALSA playback devices and exit
  --version                 print the version and exit
  -h, --help                print this help

RUST_LOG=debug logs every request a sender makes and hex-dumps the bodies.
";

/// Parse the `--mac` argument, e.g. `aa:bb:cc:dd:ee:ff`.
fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let mut mac = [0u8; 6];
    let mut parts = s.trim().split(':');
    for byte in &mut mac {
        *byte = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    parts.next().is_none().then_some(mac)
}

/// Pure over its input — no process exit, no environment — so it has tests.
/// Errors are the message alone; `main` appends the usage line.
fn parse(mut it: impl Iterator<Item = String>) -> Result<Action, String> {
    let mut args = Args {
        name: None,
        port: None,
        mac: None,
        identity_file: None,
        avahi: true,
        alsa_device: Some(DEFAULT_ALSA_DEVICE.to_string()),
        pincode: None,
        tui_listen: None,
    };
    let value = |flag: &str, it: &mut dyn Iterator<Item = String>| {
        it.next().ok_or_else(|| format!("{flag} needs a value"))
    };
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--name" => args.name = Some(value("--name", &mut it)?),
            "--port" => {
                let v = value("--port", &mut it)?;
                args.port = Some(
                    v.parse()
                        .map_err(|_| format!("--port needs a number 1-65535, not \"{v}\""))?,
                );
            }
            "--mac" => {
                let v = value("--mac", &mut it)?;
                args.mac = Some(
                    parse_mac(&v)
                        .ok_or_else(|| format!("--mac needs AA:BB:CC:DD:EE:FF, not \"{v}\""))?,
                );
            }
            "--identity-file" => {
                args.identity_file = Some(PathBuf::from(value("--identity-file", &mut it)?))
            }
            "--no-avahi" => args.avahi = false,
            "--alsa-device" => args.alsa_device = Some(value("--alsa-device", &mut it)?),
            "--no-audio" => args.alsa_device = None,
            "--pincode" => args.pincode = Some(value("--pincode", &mut it)?),
            "--tui-listen" => args.tui_listen = Some(value("--tui-listen", &mut it)?),
            "--list-devices" => return Ok(Action::ListDevices),
            "--version" => return Ok(Action::Version),
            "-h" | "--help" => return Ok(Action::Help),
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(Action::Run(args))
}

/// Make a print-and-exit path behave like a Unix tool when piped into
/// `head` or a pager: die quietly on a closed pipe instead of panicking.
/// Only for paths that exit right after printing — the server keeps Rust's
/// default (SIGPIPE ignored) so a peer closing a socket mid-write stays an
/// io error, not a process death.
fn sigpipe_default() {
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

fn default_identity_path() -> PathBuf {
    match std::env::var_os("HOME") {
        Some(home) => PathBuf::from(home).join(".config/openairplay2/identity"),
        None => PathBuf::from("openairplay2.identity"),
    }
}

/// Resolves on Ctrl-C or on SIGTERM — the latter is how systemd stops a
/// service, and without a handler it would kill the process outright.
async fn shutdown_signal() {
    let mut sigterm = match tokio::signal::unix::signal(SignalKind::terminate()) {
        Ok(sigterm) => sigterm,
        // Nothing to fall back to but Ctrl-C; the default SIGTERM disposition
        // still terminates the process, just not gracefully.
        Err(e) => {
            debug!("cannot listen for SIGTERM ({e}); Ctrl-C only");
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = sigterm.recv() => {}
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = match parse(std::env::args().skip(1)) {
        Ok(Action::Run(args)) => args,
        Ok(Action::Help) => {
            sigpipe_default();
            print!("{HELP}");
            return ExitCode::SUCCESS;
        }
        Ok(Action::Version) => {
            println!("openairplay2-receiver {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Ok(Action::ListDevices) => {
            sigpipe_default();
            return match player::list_devices() {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("error: cannot list ALSA devices: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        Err(msg) => {
            eprintln!("error: {msg}");
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    let identity_path = args.identity_file.unwrap_or_else(default_identity_path);
    let mut builder = Receiver::builder()
        .identity_path(&identity_path)
        .advertise(args.avahi);
    if let Some(name) = args.name {
        builder = builder.name(name);
    }
    if let Some(port) = args.port {
        builder = builder.port(port);
    }
    if let Some(mac) = args.mac {
        builder = builder.mac(mac);
    }
    if let Some(pincode) = args.pincode {
        builder = builder.pincode(pincode);
    }
    let receiver = match builder.build() {
        Ok(receiver) => receiver,
        Err(e) => {
            eprintln!("cannot load or create identity at {identity_path:?}: {e}");
            return ExitCode::FAILURE;
        }
    };

    info!(
        "starting AirPlay 2 receiver \"{}\" (deviceid {}, port {}, pk {})",
        receiver.config().name,
        receiver.config().device_id(),
        receiver.config().port,
        receiver.identity().public_key_hex()
    );
    // The pincode is a secret: name the state, never the value.
    match &receiver.config().pincode {
        Some(_) => info!("pincode: required (senders must enter one to pair)"),
        None => info!("pincode: off (transient 3939)"),
    }
    match &args.alsa_device {
        Some(dev) => info!("audio output: ALSA \"{dev}\""),
        None => info!("audio output: disabled (--no-audio)"),
    }

    // Probe the device now, so a typo fails in the user's face instead of
    // starting a receiver that decodes to nowhere (the sink's decode-only
    // fallback is for devices that vanish mid-run, not for wrong names).
    if let Some(dev) = &args.alsa_device {
        match player::probe_device(dev) {
            player::Probe::Ok => {}
            player::Probe::Warn(e) => {
                warn!("cannot open ALSA \"{dev}\" right now ({e}); will try again when a stream starts")
            }
            player::Probe::Fatal(msg) => {
                eprintln!("error: {msg}");
                return ExitCode::FAILURE;
            }
        }
    }

    // The sink seam: the library delivers PCM to an AlsaSink per stream and
    // reports session events; the volume path is ours (dB → linear gain,
    // shared with the sink so slider moves apply live).
    let gain = SharedGain::new();
    let sink_gain = gain.clone();
    let device = args.alsa_device;
    let sink_factory = move |rate: u32, channels: u8| -> Box<dyn AudioSink> {
        match &device {
            Some(dev) => Box::new(AlsaSink::open(dev, rate, channels, sink_gain.clone())),
            None => Box::new(NullSink),
        }
    };
    // The now-playing endpoint, if asked for. Bound before streaming starts so
    // a bad address fails at startup rather than mid-session.
    let publisher = match &args.tui_listen {
        Some(addr) => {
            let listener = match tokio::net::TcpListener::bind(addr).await {
                Ok(listener) => listener,
                Err(e) => {
                    eprintln!("cannot listen for displays on {addr}: {e}");
                    return ExitCode::FAILURE;
                }
            };
            info!("now-playing endpoint: ws://{addr}");
            let publisher = tui::Publisher::new(receiver.config().name.clone());
            tokio::spawn(tui::serve(listener, publisher.clone()));
            Some(publisher)
        }
        None => None,
    };

    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        while let Some(event) = event_rx.recv().await {
            // Events drive our gain (always) and the display socket (when
            // serving). The publisher never blocks on a slow display.
            if let Some(publisher) = &publisher {
                publisher.publish(&event);
            }
            match event {
                Event::Volume { db } => {
                    debug!("volume {db} dB");
                    gain.set(volume_to_gain(db));
                }
                Event::SessionStarted {
                    rate,
                    channels,
                    peer,
                } => {
                    debug!("session started from {peer} ({rate} Hz, {channels}ch)");
                }
                Event::Progress { elapsed, duration } => {
                    debug!(
                        "progress {:.0}s / {:.0}s",
                        elapsed.as_secs_f32(),
                        duration.as_secs_f32()
                    );
                }
                Event::SessionEnded => debug!("session ended"),
                Event::Metadata {
                    title,
                    artist,
                    album,
                } => {
                    let field = |v: Option<String>| v.unwrap_or_else(|| "-".into());
                    debug!(
                        "now playing: {} — {} ({})",
                        field(artist),
                        field(title),
                        field(album)
                    );
                }
                Event::Artwork { content_type, data } => {
                    debug!("artwork: {content_type}, {} bytes", data.len());
                }
                Event::Paused(paused) => debug!("paused: {paused}"),
                Event::Flushed => debug!("flushed"),
                _ => {}
            }
        }
    });

    tokio::select! {
        result = receiver.run(sink_factory, event_tx) => {
            if let Err(e) = result {
                if e.kind() == std::io::ErrorKind::AddrInUse {
                    eprintln!("error: {e} — is another receiver already running?");
                } else {
                    eprintln!("error: {e}");
                }
                return ExitCode::FAILURE;
            }
        }
        _ = shutdown_signal() => {
            info!("shutting down");
        }
    }
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_strs(args: &[&str]) -> Result<Action, String> {
        parse(args.iter().map(|s| s.to_string()))
    }

    fn run_args(args: &[&str]) -> Args {
        match parse_strs(args) {
            Ok(Action::Run(args)) => args,
            other => panic!("expected Run, got {other:?}"),
        }
    }

    #[test]
    fn no_arguments_yields_the_defaults() {
        let args = run_args(&[]);
        assert_eq!(args.name, None);
        assert_eq!(args.port, None);
        assert_eq!(args.mac, None);
        assert_eq!(args.identity_file, None);
        assert!(args.avahi);
        assert_eq!(args.alsa_device.as_deref(), Some(DEFAULT_ALSA_DEVICE));
        assert_eq!(args.pincode, None);
        assert_eq!(args.tui_listen, None);
    }

    #[test]
    fn every_flag_parses() {
        let args = run_args(&[
            "--name",
            "Kitchen",
            "--port",
            "7100",
            "--mac",
            "aa:bb:cc:dd:ee:ff",
            "--identity-file",
            "/tmp/id",
            "--no-avahi",
            "--alsa-device",
            "hw:1",
            "--pincode",
            "4821",
            "--tui-listen",
            "127.0.0.1:7392",
        ]);
        assert_eq!(args.name.as_deref(), Some("Kitchen"));
        assert_eq!(args.port, Some(7100));
        assert_eq!(args.mac, Some([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]));
        assert_eq!(
            args.identity_file.as_deref(),
            Some(std::path::Path::new("/tmp/id"))
        );
        assert!(!args.avahi);
        assert_eq!(args.alsa_device.as_deref(), Some("hw:1"));
        assert_eq!(args.pincode.as_deref(), Some("4821"));
        assert_eq!(args.tui_listen.as_deref(), Some("127.0.0.1:7392"));
    }

    #[test]
    fn later_flags_win() {
        // --no-audio clears the device; a later --alsa-device restores one.
        assert_eq!(run_args(&["--no-audio"]).alsa_device, None);
        assert_eq!(
            run_args(&["--no-audio", "--alsa-device", "hw:0"])
                .alsa_device
                .as_deref(),
            Some("hw:0")
        );
        assert_eq!(
            run_args(&["--alsa-device", "hw:0", "--no-audio"]).alsa_device,
            None
        );
    }

    #[test]
    fn help_version_and_list_devices_are_actions() {
        assert_eq!(parse_strs(&["--help"]), Ok(Action::Help));
        assert_eq!(parse_strs(&["-h"]), Ok(Action::Help));
        assert_eq!(parse_strs(&["--version"]), Ok(Action::Version));
        assert_eq!(parse_strs(&["--list-devices"]), Ok(Action::ListDevices));
    }

    #[test]
    fn mistakes_name_the_flag() {
        assert!(parse_strs(&["--name"]).unwrap_err().contains("--name"));
        assert!(parse_strs(&["--port", "x"]).unwrap_err().contains("--port"));
        assert!(parse_strs(&["--port", "70000"])
            .unwrap_err()
            .contains("--port"));
        assert!(parse_strs(&["--mac", "not-a-mac"])
            .unwrap_err()
            .contains("--mac"));
        assert!(parse_strs(&["--frobnicate"])
            .unwrap_err()
            .contains("--frobnicate"));
    }

    /// Every flag the parser accepts is documented in HELP — adding a flag
    /// without describing it fails here. (The list is checked against the
    /// parser too: each entry must parse.)
    #[test]
    fn help_describes_every_flag() {
        let flags: &[&[&str]] = &[
            &["--name", "x"],
            &["--port", "7000"],
            &["--mac", "aa:bb:cc:dd:ee:ff"],
            &["--identity-file", "x"],
            &["--pincode", "x"],
            &["--no-avahi"],
            &["--alsa-device", "x"],
            &["--no-audio"],
            &["--tui-listen", "x"],
            &["--list-devices"],
            &["--version"],
            &["--help"],
        ];
        for invocation in flags {
            assert!(
                parse_strs(invocation).is_ok(),
                "{invocation:?} should parse"
            );
            assert!(
                HELP.contains(invocation[0]),
                "HELP is missing {}",
                invocation[0]
            );
        }
    }
}
