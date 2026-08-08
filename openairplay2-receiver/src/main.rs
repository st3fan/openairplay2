//! The standalone Linux/ALSA AirPlay 2 receiver: a CLI over the
//! `openairplay2` library's public API (it is embedder #1), with an ALSA
//! sink and the dB → linear gain volume model.

use std::path::PathBuf;
use std::process::ExitCode;

use log::{debug, error, info, warn};
use tokio::signal::unix::SignalKind;

mod player;
mod tui;

use crate::player::{volume_to_gain, AlsaSink, NullSink, SharedGain};
use openairplay2::{AudioSink, Event, Receiver};

const DEFAULT_ALSA_DEVICE: &str = "default";

/// Everything is optional: `None` means "not given on the command line", so
/// [`resolve`] knows which fields the environment may still fill before the
/// built-in defaults apply.
#[derive(Debug, PartialEq)]
struct Args {
    /// `None` → the library's defaults (name "OpenAirPlay2", port 7000).
    name: Option<String>,
    port: Option<u16>,
    mac: Option<[u8; 6]>,
    identity_file: Option<PathBuf>,
    avahi: Option<bool>,
    /// Audio on or off (`--no-audio` / `OPENAIRPLAY2_AUDIO`); `None` → on.
    audio: Option<bool>,
    alsa_device: Option<String>,
    /// Require this pincode to pair; `None` → transient `3939`.
    pincode: Option<String>,
    /// Address to serve the now-playing WebSocket on; off when `None`.
    tui_listen: Option<String>,
}

/// The device to open after flags and environment have merged: `None` is
/// no audio at all. The audio switch dominates the device name, and an
/// enabled output falls back to the default device.
fn effective_device(args: &Args) -> Option<String> {
    if !args.audio.unwrap_or(true) {
        return None;
    }
    Some(
        args.alsa_device
            .clone()
            .unwrap_or_else(|| DEFAULT_ALSA_DEVICE.to_string()),
    )
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

Each option can also come from the environment — OPENAIRPLAY2_NAME, _PORT,
_MAC, _IDENTITY_FILE, _PINCODE, _AVAHI (on/off), _AUDIO (on/off),
_ALSA_DEVICE, _TUI_LISTEN — which is how /etc/default/openairplay2-receiver
configures the service. A flag wins over its variable; an empty variable is
unset. %h in the name becomes this machine's hostname.

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
        avahi: None,
        audio: None,
        alsa_device: None,
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
                args.port = Some(port_value(&v).map_err(|e| format!("--port {e}"))?);
            }
            "--mac" => {
                let v = value("--mac", &mut it)?;
                args.mac = Some(mac_value(&v).map_err(|e| format!("--mac {e}"))?);
            }
            "--identity-file" => {
                args.identity_file = Some(PathBuf::from(value("--identity-file", &mut it)?))
            }
            "--no-avahi" => args.avahi = Some(false),
            // The pair of flags keeps its later-flag-wins behavior: naming a
            // device turns audio back on, and --no-audio turns it off.
            "--alsa-device" => {
                args.alsa_device = Some(value("--alsa-device", &mut it)?);
                args.audio = Some(true);
            }
            "--no-audio" => args.audio = Some(false),
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

/// The value validators, shared by the flags and the environment variables so
/// `OPENAIRPLAY2_PORT=x` fails exactly like `--port x`; the caller prefixes
/// the flag or variable name.
fn port_value(v: &str) -> Result<u16, String> {
    v.parse()
        .map_err(|_| format!("needs a number 1-65535, not \"{v}\""))
}

fn mac_value(v: &str) -> Result<[u8; 6], String> {
    parse_mac(v).ok_or_else(|| format!("needs AA:BB:CC:DD:EE:FF, not \"{v}\""))
}

fn on_off_value(v: &str) -> Result<bool, String> {
    match v {
        "on" => Ok(true),
        "off" => Ok(false),
        _ => Err(format!("must be \"on\" or \"off\", not \"{v}\"")),
    }
}

/// Fill whatever the command line left unset from the environment — how
/// `/etc/default/openairplay2-receiver` configures the service. A flag wins
/// over its variable; an empty variable is unset (an options-file line edited
/// to `OPENAIRPLAY2_NAME=` means "default"). Pure over the lookup closure so
/// it has tests.
fn resolve(mut args: Args, env: impl Fn(&str) -> Option<String>) -> Result<Args, String> {
    let get = |k: &str| env(k).filter(|v| !v.is_empty());
    if args.name.is_none() {
        args.name = get("OPENAIRPLAY2_NAME");
    }
    if args.port.is_none() {
        if let Some(v) = get("OPENAIRPLAY2_PORT") {
            args.port = Some(port_value(&v).map_err(|e| format!("OPENAIRPLAY2_PORT {e}"))?);
        }
    }
    if args.mac.is_none() {
        if let Some(v) = get("OPENAIRPLAY2_MAC") {
            args.mac = Some(mac_value(&v).map_err(|e| format!("OPENAIRPLAY2_MAC {e}"))?);
        }
    }
    if args.identity_file.is_none() {
        args.identity_file = get("OPENAIRPLAY2_IDENTITY_FILE").map(PathBuf::from);
    }
    if args.avahi.is_none() {
        if let Some(v) = get("OPENAIRPLAY2_AVAHI") {
            args.avahi = Some(on_off_value(&v).map_err(|e| format!("OPENAIRPLAY2_AVAHI {e}"))?);
        }
    }
    if args.audio.is_none() {
        if let Some(v) = get("OPENAIRPLAY2_AUDIO") {
            args.audio = Some(on_off_value(&v).map_err(|e| format!("OPENAIRPLAY2_AUDIO {e}"))?);
        }
    }
    if args.alsa_device.is_none() {
        args.alsa_device = get("OPENAIRPLAY2_ALSA_DEVICE");
    }
    if args.pincode.is_none() {
        args.pincode = get("OPENAIRPLAY2_PINCODE");
    }
    if args.tui_listen.is_none() {
        args.tui_listen = get("OPENAIRPLAY2_TUI_LISTEN");
    }
    Ok(args)
}

/// The 0.4 migration tripwire: the options file used to hold one
/// `OPENAIRPLAY2_ARGS` blob of command-line flags, and dpkg preserves a
/// locally edited conffile — so a box upgraded across the change still sets
/// it, and silently running with defaults would be the worst outcome. The
/// message is logged at error level; the receiver keeps running, because a
/// receiver with a default name still plays and a refused start plays
/// nothing.
fn legacy_args_notice(env: impl Fn(&str) -> Option<String>) -> Option<String> {
    env("OPENAIRPLAY2_ARGS")
        .filter(|v| !v.trim().is_empty())
        .map(|_| {
            "OPENAIRPLAY2_ARGS is set but no longer read: options are now one \
             OPENAIRPLAY2_* variable each in /etc/default/openairplay2-receiver \
             — see NEWS.Debian or the README, and migrate; running with what \
             is configured otherwise"
                .to_string()
        })
}

/// `%h` → this machine's hostname, `%%` → a literal `%`; anything else passes
/// through. One options file can then be deployed unedited across machines.
fn substitute(name: &str, hostname: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut chars = name.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('h') => out.push_str(hostname),
            Some('%') => out.push('%'),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
}

fn hostname() -> Option<String> {
    let mut buf = [0u8; 256];
    if unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } != 0 {
        return None;
    }
    let end = buf.iter().position(|&b| b == 0)?;
    String::from_utf8(buf[..end].to_vec())
        .ok()
        .filter(|s| !s.is_empty())
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
    // The environment fills what the flags left unset — this is how the
    // options file configures the service.
    let args = match resolve(args, |k| std::env::var(k).ok()) {
        Ok(args) => args,
        Err(msg) => {
            eprintln!("error: {msg}");
            return ExitCode::FAILURE;
        }
    };
    if let Some(msg) = legacy_args_notice(|k| std::env::var(k).ok()) {
        error!("{msg}");
    }
    let alsa_device = effective_device(&args);

    let identity_path = args.identity_file.unwrap_or_else(default_identity_path);
    let mut builder = Receiver::builder()
        .identity_path(&identity_path)
        .advertise(args.avahi.unwrap_or(true));
    if let Some(name) = args.name {
        // %h in a name becomes the hostname (skipped if there is none to be
        // had — a literal %h beats losing the name).
        let name = match hostname() {
            Some(host) => substitute(&name, &host),
            None => name,
        };
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
    match &alsa_device {
        Some(dev) => info!("audio output: ALSA \"{dev}\""),
        None => info!("audio output: disabled (--no-audio)"),
    }

    // Probe the device now, so a typo fails in the user's face instead of
    // starting a receiver that decodes to nowhere (the sink's decode-only
    // fallback is for devices that vanish mid-run, not for wrong names).
    if let Some(dev) = &alsa_device {
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
    let device = alsa_device;
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

    /// Environment lookup for tests: a slice of pairs.
    fn env_of<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            pairs
                .iter()
                .find(|(name, _)| *name == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn no_arguments_yields_the_defaults() {
        let args = run_args(&[]);
        assert_eq!(args.name, None);
        assert_eq!(args.port, None);
        assert_eq!(args.mac, None);
        assert_eq!(args.identity_file, None);
        assert_eq!(args.avahi, None);
        assert_eq!(args.audio, None);
        assert_eq!(args.alsa_device, None);
        assert_eq!(args.pincode, None);
        assert_eq!(args.tui_listen, None);
        // The effective outcome: audio on, default device.
        assert_eq!(
            effective_device(&args).as_deref(),
            Some(DEFAULT_ALSA_DEVICE)
        );
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
        assert_eq!(args.avahi, Some(false));
        assert_eq!(effective_device(&args).as_deref(), Some("hw:1"));
        assert_eq!(args.pincode.as_deref(), Some("4821"));
        assert_eq!(args.tui_listen.as_deref(), Some("127.0.0.1:7392"));
    }

    #[test]
    fn later_flags_win() {
        // --no-audio silences; a later --alsa-device turns audio back on.
        assert_eq!(effective_device(&run_args(&["--no-audio"])), None);
        assert_eq!(
            effective_device(&run_args(&["--no-audio", "--alsa-device", "hw:0"])).as_deref(),
            Some("hw:0")
        );
        assert_eq!(
            effective_device(&run_args(&["--alsa-device", "hw:0", "--no-audio"])),
            None
        );
    }

    #[test]
    fn environment_fills_what_flags_left_unset() {
        let env = [
            ("OPENAIRPLAY2_NAME", "Kitchen %h"),
            ("OPENAIRPLAY2_PORT", "7100"),
            ("OPENAIRPLAY2_MAC", "aa:bb:cc:dd:ee:ff"),
            ("OPENAIRPLAY2_IDENTITY_FILE", "/var/lib/x/identity"),
            ("OPENAIRPLAY2_PINCODE", "4821"),
            ("OPENAIRPLAY2_AVAHI", "off"),
            ("OPENAIRPLAY2_AUDIO", "off"),
            ("OPENAIRPLAY2_ALSA_DEVICE", "hw:1"),
            ("OPENAIRPLAY2_TUI_LISTEN", "0.0.0.0:7392"),
        ];
        let args = resolve(run_args(&[]), env_of(&env)).unwrap();
        assert_eq!(args.name.as_deref(), Some("Kitchen %h"));
        assert_eq!(args.port, Some(7100));
        assert_eq!(args.mac, Some([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]));
        assert_eq!(
            args.identity_file.as_deref(),
            Some(std::path::Path::new("/var/lib/x/identity"))
        );
        assert_eq!(args.pincode.as_deref(), Some("4821"));
        assert_eq!(args.avahi, Some(false));
        assert_eq!(args.tui_listen.as_deref(), Some("0.0.0.0:7392"));
        // The audio switch dominates the device name.
        assert_eq!(effective_device(&args), None);
    }

    #[test]
    fn the_flag_wins_over_the_variable() {
        let env = [
            ("OPENAIRPLAY2_NAME", "FromEnv"),
            ("OPENAIRPLAY2_AUDIO", "off"),
        ];
        let args = resolve(
            run_args(&["--name", "FromFlag", "--alsa-device", "hw:2"]),
            env_of(&env),
        )
        .unwrap();
        assert_eq!(args.name.as_deref(), Some("FromFlag"));
        // --alsa-device turned audio on; the variable does not override it.
        assert_eq!(effective_device(&args).as_deref(), Some("hw:2"));
    }

    #[test]
    fn an_empty_variable_is_unset() {
        let env = [("OPENAIRPLAY2_NAME", ""), ("OPENAIRPLAY2_PORT", "")];
        let args = resolve(run_args(&[]), env_of(&env)).unwrap();
        assert_eq!(args.name, None);
        assert_eq!(args.port, None);
    }

    #[test]
    fn bad_variable_values_name_the_variable() {
        let cases = [
            ("OPENAIRPLAY2_PORT", "x"),
            ("OPENAIRPLAY2_MAC", "not-a-mac"),
            ("OPENAIRPLAY2_AVAHI", "yes"),
            ("OPENAIRPLAY2_AUDIO", "1"),
        ];
        for (var, value) in cases {
            let env = [(var, value)];
            let err = resolve(run_args(&[]), env_of(&env)).unwrap_err();
            assert!(err.contains(var), "{err}");
            assert!(err.contains(value), "{err}");
        }
    }

    #[test]
    fn legacy_args_trips_only_when_set_and_nonempty() {
        assert!(legacy_args_notice(env_of(&[])).is_none());
        assert!(legacy_args_notice(env_of(&[("OPENAIRPLAY2_ARGS", " ")])).is_none());
        let msg = legacy_args_notice(env_of(&[("OPENAIRPLAY2_ARGS", "--name X")])).unwrap();
        assert!(msg.contains("no longer read"), "{msg}");
    }

    #[test]
    fn name_substitution() {
        assert_eq!(substitute("Kitchen", "pi"), "Kitchen");
        assert_eq!(substitute("%h", "pi"), "pi");
        assert_eq!(substitute("Music (%h)", "pi"), "Music (pi)");
        assert_eq!(substitute("100%% %h", "pi"), "100% pi");
        assert_eq!(substitute("50%", "pi"), "50%");
        assert_eq!(substitute("%x", "pi"), "%x");
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
