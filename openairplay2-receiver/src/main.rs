//! The standalone Linux/ALSA AirPlay 2 receiver: a CLI over the
//! `openairplay2` library's public API (it is embedder #1), with an ALSA
//! sink and the dB → linear gain volume model.

use std::path::PathBuf;
use std::process::ExitCode;

use log::{debug, error, info, warn};
use tokio::signal::unix::SignalKind;

mod mixer;
mod player;
mod tui;

use crate::player::{volume_to_gain, AlsaSink, NullSink, SharedGain};
use openairplay2::{AudioSink, Event, Receiver};

const DEFAULT_ALSA_DEVICE: &str = "default";

/// Exit code for a configuration mistake the operator must fix — a bad
/// `OPENAIRPLAY2_*` value, an unknown ALSA device, a port already in use or
/// privileged. `78` is `EX_CONFIG` from sysexits.h. The systemd unit lists it
/// in `RestartPreventExitStatus`, so such an exit fails the service and stays
/// stopped rather than restart-looping; a crash (any other failure, or a
/// signal) still restarts.
const EXIT_CONFIG: u8 = 78;

/// Everything is optional: `None` means "not given on the command line", so
/// [`resolve`] knows which fields the environment may still fill before the
/// built-in defaults apply.
#[derive(Debug, PartialEq)]
struct Args {
    /// `None` → [`DEFAULT_NAME`] (`display_name`); port falls back to the
    /// library default (7000).
    name: Option<String>,
    port: Option<u16>,
    mac: Option<[u8; 6]>,
    identity_file: Option<PathBuf>,
    avahi: Option<bool>,
    /// Audio on or off (`--no-audio` / `OPENAIRPLAY2_AUDIO`); `None` → on.
    audio: Option<bool>,
    alsa_device: Option<String>,
    /// Drive this ALSA mixer control from volume events instead of scaling
    /// samples in software; `None` → software gain.
    mixer: Option<String>,
    /// The mixer device holding that control; `None` → derived from the
    /// audio device (see [`default_mixer_device`]).
    mixer_device: Option<String>,
    /// Require this password to pair; `None` → transient `3939`.
    password: Option<String>,
    /// The local now-playing Unix socket: a path, `off`, or `None` for the
    /// default path (see [`tui_socket_path`]).
    tui_socket: Option<String>,
    /// Address to serve the now-playing WebSocket on; off when `None`.
    tui_listen: Option<String>,
    /// Require this password on the now-playing WebSocket; `None` → open.
    tui_password: Option<String>,
    /// Log verbosity (`error`/`warn`/`info`/`debug`/`trace`); `None` → `info`.
    log_level: Option<String>,
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

/// The hardware-volume configuration after flags and environment have
/// merged: `Some((device, control))` when a mixer control should follow the
/// sender's volume, `None` for the software gain. A mixer device without a
/// control to drive on it is a config mistake, not a default.
fn mixer_config(args: &Args) -> Result<Option<(String, String)>, String> {
    let control = match (&args.mixer, &args.mixer_device) {
        (Some(control), _) => control,
        (None, None) => return Ok(None),
        (None, Some(_)) => {
            return Err("--mixer-device (OPENAIRPLAY2_MIXER_DEVICE) needs --mixer \
                 (OPENAIRPLAY2_MIXER) to name the control to drive"
                .to_string())
        }
    };
    let device = match &args.mixer_device {
        Some(device) => device.clone(),
        None => default_mixer_device(effective_device(args).as_deref()),
    };
    Ok(Some((device, control.clone())))
}

/// The mixer device when none is configured: the card of the audio device
/// (`plughw:CARD=S2` → `hw:CARD=S2`), because the control being driven is
/// almost always on the card being played to; ALSA's `default` otherwise
/// (where PulseAudio/PipeWire expose their own `Master`).
fn default_mixer_device(alsa_device: Option<&str>) -> String {
    alsa_device
        .and_then(player::card_id_of)
        .map(|id| format!("hw:CARD={id}"))
        .unwrap_or_else(|| "default".to_string())
}

/// Where the now-playing Unix socket goes. `--tui-socket`: a path is bound
/// exactly there (failing to is a config error, like a bad `--alsa-device`),
/// `off` serves none, and unset means the default — the per-user runtime
/// directory when there is one (a manual run), the system one otherwise
/// (what `RuntimeDirectory=` gives the service) — where failing to bind
/// only warns: it is a default, not a request, and a receiver that plays
/// but has no local display socket beats one that refuses to start. The
/// bool carries that explicit/default distinction to the caller.
fn tui_socket_path(
    configured: Option<&str>,
    xdg_runtime_dir: Option<&str>,
) -> Option<(PathBuf, bool)> {
    match configured {
        Some("off") => None,
        Some(path) => Some((PathBuf::from(path), true)),
        None => {
            let base = match xdg_runtime_dir {
                Some(dir) if !dir.is_empty() => PathBuf::from(dir),
                _ => PathBuf::from("/run"),
            };
            Some((base.join("openairplay2").join("tui.sock"), false))
        }
    }
}

/// What the command line asks for; everything but `Run` prints and exits.
/// `Args` is boxed only to keep the variants comparable in size (clippy's
/// `large_enum_variant`).
#[derive(Debug, PartialEq)]
enum Action {
    Run(Box<Args>),
    Help,
    Version,
    ListDevices,
    ListAllDevices,
    ListMixers,
}

const USAGE: &str = "usage: openairplay2-receiver [options] — run with --help for the list";

const HELP: &str = "\
openairplay2-receiver — standalone AirPlay 2 audio receiver (ALSA)

usage: openairplay2-receiver [options]

options:
  --name NAME               name senders see in the AirPlay menu; %h in it
                            becomes the hostname
                            (default \"OpenAirPlay2 (%h)\")
  --port PORT               control port (default 7000)
  --mac AA:BB:CC:DD:EE:FF   device id in the advertisement (default: taken
                            from a real network interface)
  --identity-file PATH      where the pairing identity lives — senders
                            remember this receiver by it, so keep it stable
                            (default ~/.config/openairplay2/identity)
  --password PASS           require this password to pair — iOS/macOS show a
                            password dialog (alphanumerics welcome); without
                            one any device on the network can pair. Prefer
                            the OPENAIRPLAY2_PASSWORD variable — a flag is
                            visible in ps. (--pincode is the deprecated 0.4
                            spelling of the same option)
  --no-avahi                do not advertise over mDNS
  --alsa-device NAME        ALSA playback device (default \"default\");
                            see --list-devices for what this machine has
  --no-audio                decode but do not open ALSA (silent test run)
  --mixer CONTROL           drive this ALSA mixer control from the sender's
                            volume instead of scaling samples in software —
                            keeps the full sample resolution at low volume,
                            and a DAC or amp with its own volume control
                            follows the slider; NAME or NAME,INDEX, see
                            --list-mixers for this machine's controls
  --mixer-device DEV        mixer device holding that control (default: the
                            card of --alsa-device, else \"default\")
  --tui-socket PATH|off     where to serve the local now-playing socket
                            that openairplay2-tui connects to by default
                            (default $XDG_RUNTIME_DIR/openairplay2/tui.sock,
                            else /run/openairplay2/tui.sock); off serves
                            none. Any local user may connect — the socket
                            file's permissions are the access control
  --tui-listen ADDR         serve the now-playing WebSocket that
                            openairplay2-tui renders, e.g. 127.0.0.1:7392; it
                            carries track metadata and cover art, so keep it
                            on loopback unless you mean otherwise
  --tui-password PASS       require this password on the now-playing
                            WebSocket (openairplay2-tui --password); without
                            one, anyone who can reach the address connects.
                            Prefer the OPENAIRPLAY2_TUI_PASSWORD variable —
                            like --pincode, a flag is visible in ps
  --log-level LEVEL         error, warn, info, debug or trace (default info);
                            debug logs every request a sender makes and
                            hex-dumps the bodies
  --debug                   shorthand for --log-level debug
  --list-devices            list the audio outputs (one per sound card) and
                            exit — pass one to --alsa-device
  --list-all-devices        list every ALSA playback device, including
                            hardware sub-devices and plugins, and exit
  --list-mixers             list the mixer volume controls of each device
                            and exit — pass one to --mixer
  --version                 print the version and exit
  -h, --help                print this help

Each option can also come from the environment — OPENAIRPLAY2_NAME, _PORT,
_MAC, _IDENTITY_FILE, _PASSWORD, _AVAHI (on/off), _AUDIO (on/off),
_ALSA_DEVICE, _MIXER, _MIXER_DEVICE, _TUI_SOCKET, _TUI_LISTEN,
_TUI_PASSWORD, _LOG_LEVEL — which is how
/etc/default/openairplay2-receiver configures the service. A flag wins over its
variable; an empty variable is unset. %h in the name becomes this machine's
hostname.

RUST_LOG overrides --log-level for per-module control, e.g.
RUST_LOG=openairplay2::session=trace.
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
        mixer: None,
        mixer_device: None,
        password: None,
        tui_socket: None,
        tui_listen: None,
        tui_password: None,
        log_level: None,
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
            "--mixer" => args.mixer = Some(value("--mixer", &mut it)?),
            "--mixer-device" => args.mixer_device = Some(value("--mixer-device", &mut it)?),
            "--password" => args.password = Some(value("--password", &mut it)?),
            // The 0.4 spelling; kept so nobody's pairing protection silently
            // vanishes on upgrade.
            "--pincode" => args.password = Some(value("--pincode", &mut it)?),
            "--tui-socket" => args.tui_socket = Some(value("--tui-socket", &mut it)?),
            "--tui-listen" => args.tui_listen = Some(value("--tui-listen", &mut it)?),
            "--tui-password" => args.tui_password = Some(value("--tui-password", &mut it)?),
            "--log-level" => {
                let v = value("--log-level", &mut it)?;
                args.log_level = Some(log_level_value(&v).map_err(|e| format!("--log-level {e}"))?);
            }
            // Sugar for the common case; later-flag-wins with --log-level.
            "--debug" => args.log_level = Some("debug".to_string()),
            "--list-devices" => return Ok(Action::ListDevices),
            "--list-all-devices" => return Ok(Action::ListAllDevices),
            "--list-mixers" => return Ok(Action::ListMixers),
            "--version" => return Ok(Action::Version),
            "-h" | "--help" => return Ok(Action::Help),
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(Action::Run(Box::new(args)))
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

/// Normalize a log level, so `--log-level DEBUG` and `debug` are the same, and
/// a nonsense level is rejected rather than silently disabling logging.
fn log_level_value(v: &str) -> Result<String, String> {
    let level = v.to_ascii_lowercase();
    match level.as_str() {
        "error" | "warn" | "info" | "debug" | "trace" => Ok(level),
        _ => Err(format!(
            "must be error, warn, info, debug or trace, not \"{v}\""
        )),
    }
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
    if args.mixer.is_none() {
        args.mixer = get("OPENAIRPLAY2_MIXER");
    }
    if args.mixer_device.is_none() {
        args.mixer_device = get("OPENAIRPLAY2_MIXER_DEVICE");
    }
    if args.password.is_none() {
        args.password = get("OPENAIRPLAY2_PASSWORD");
    }
    // The 0.4 variable: dpkg preserves an edited options file across the
    // rename, and a box that configured a pairing code must keep requiring
    // it. The new name wins when both are set.
    if args.password.is_none() {
        args.password = get("OPENAIRPLAY2_PINCODE");
    }
    if args.tui_socket.is_none() {
        args.tui_socket = get("OPENAIRPLAY2_TUI_SOCKET");
    }
    if args.tui_listen.is_none() {
        args.tui_listen = get("OPENAIRPLAY2_TUI_LISTEN");
    }
    if args.tui_password.is_none() {
        args.tui_password = get("OPENAIRPLAY2_TUI_PASSWORD");
    }
    if args.log_level.is_none() {
        if let Some(v) = get("OPENAIRPLAY2_LOG_LEVEL") {
            args.log_level =
                Some(log_level_value(&v).map_err(|e| format!("OPENAIRPLAY2_LOG_LEVEL {e}"))?);
        }
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

/// 0.5 renamed the pairing pincode to "password" — Apple's own word: iOS and
/// macOS show a password dialog and accept alphanumerics. The old variable
/// keeps working (see [`resolve`]) so an upgraded box keeps its protection,
/// but say so, so options files migrate. Warn level: honored, not ignored —
/// unlike `OPENAIRPLAY2_ARGS`.
fn legacy_pincode_notice(env: impl Fn(&str) -> Option<String>) -> Option<String> {
    env("OPENAIRPLAY2_PINCODE")
        .filter(|v| !v.trim().is_empty())
        .map(|_| {
            "OPENAIRPLAY2_PINCODE is deprecated (still honored): rename it to \
             OPENAIRPLAY2_PASSWORD in /etc/default/openairplay2-receiver"
                .to_string()
        })
}

/// The advertised name when none is configured. `%h` expands to the hostname
/// (see [`display_name`]), so out of the box each receiver is distinguishable
/// on a network of several — "OpenAirPlay2 (kitchen-pi)", not a wall of
/// identical "OpenAirPlay2".
const DEFAULT_NAME: &str = "OpenAirPlay2 (%h)";

/// The name to advertise: the configured name, or [`DEFAULT_NAME`], with `%h`
/// expanded to `hostname`. With no hostname to expand, the default drops to a
/// bare "OpenAirPlay2" rather than showing literal "(%h)", while an explicit
/// name is left exactly as the user wrote it.
fn display_name(configured: Option<String>, hostname: Option<String>) -> String {
    match (configured, hostname) {
        (Some(name), Some(host)) => substitute(&name, &host),
        (Some(name), None) => name,
        (None, Some(host)) => substitute(DEFAULT_NAME, &host),
        (None, None) => "OpenAirPlay2".to_string(),
    }
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

/// Turn a device-listing result into a process exit.
fn list_result(result: Result<(), alsa::Error>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: cannot list ALSA devices: {e}");
            ExitCode::FAILURE
        }
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
    let args = match parse(std::env::args().skip(1)) {
        Ok(Action::Run(args)) => *args,
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
            return list_result(player::list_devices());
        }
        Ok(Action::ListAllDevices) => {
            sigpipe_default();
            return list_result(player::list_all_devices());
        }
        Ok(Action::ListMixers) => {
            sigpipe_default();
            return list_result(mixer::list_mixers());
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
            return ExitCode::from(EXIT_CONFIG);
        }
    };
    // Initialize logging now that the level is known: --log-level / --debug /
    // OPENAIRPLAY2_LOG_LEVEL set the default, and RUST_LOG still overrides it
    // (for per-module control like `openairplay2::session=trace`). Argument
    // errors above went to stderr, so nothing was logged before this.
    let level = args.log_level.as_deref().unwrap_or("info");
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(level)).init();

    if let Some(msg) = legacy_args_notice(|k| std::env::var(k).ok()) {
        error!("{msg}");
    }
    if let Some(msg) = legacy_pincode_notice(|k| std::env::var(k).ok()) {
        warn!("{msg}");
    }
    let alsa_device = effective_device(&args);
    // Resolved before `args` fields start moving into the builder below.
    let mixer_cfg = match mixer_config(&args) {
        Ok(cfg) => cfg,
        Err(msg) => {
            eprintln!("error: {msg}");
            return ExitCode::from(EXIT_CONFIG);
        }
    };

    let identity_path = args.identity_file.unwrap_or_else(default_identity_path);
    let mut builder = Receiver::builder()
        .identity_path(&identity_path)
        .advertise(args.avahi.unwrap_or(true));
    // Always set the name: the default itself carries the hostname now, so the
    // library's plain fallback is never what a receiver advertises.
    builder = builder.name(display_name(args.name, hostname()));
    if let Some(port) = args.port {
        builder = builder.port(port);
    }
    if let Some(mac) = args.mac {
        builder = builder.mac(mac);
    }
    if let Some(password) = args.password {
        builder = builder.password(password);
    }
    let receiver = match builder.build() {
        Ok(receiver) => receiver,
        Err(e) => {
            eprintln!("cannot load or create identity at {identity_path:?}: {e}");
            return ExitCode::from(EXIT_CONFIG);
        }
    };

    info!(
        "starting AirPlay 2 receiver \"{}\" (deviceid {}, port {}, pk {})",
        receiver.config().name,
        receiver.config().device_id(),
        receiver.config().port,
        receiver.identity().public_key_hex()
    );
    // The password is a secret: name the state, never the value.
    match &receiver.config().password {
        Some(_) => info!("password: required (senders must enter it to pair)"),
        None => info!("password: none (open pairing, transient code 3939)"),
    }
    match &alsa_device {
        Some(dev) => match player::card_name(dev) {
            Some(name) => info!("audio output: {dev} ({name})"),
            None => info!("audio output: {dev}"),
        },
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
                return ExitCode::from(EXIT_CONFIG);
            }
        }
    }

    // The sink seam: the library delivers PCM to an AlsaSink per stream and
    // reports session events; the volume path is ours (dB → linear gain,
    // shared with the sink so slider moves apply live).
    let gain = SharedGain::new();
    // Hardware volume, if configured: the mixer control follows the sender's
    // slider and the software gain stays parked at full scale. A control
    // that doesn't exist fails here, at startup, like a wrong device name.
    let mut hw_volume = match &mixer_cfg {
        Some((device, control)) => match mixer::HwVolume::open(device, control, gain.clone()) {
            Ok(hw) => {
                info!("hardware volume: {}", hw.describe());
                Some(hw)
            }
            Err(msg) => {
                eprintln!("error: {msg}");
                return ExitCode::from(EXIT_CONFIG);
            }
        },
        None => None,
    };
    let sink_gain = gain.clone();
    let device = alsa_device;
    let sink_factory = move |rate: u32, channels: u8| -> Box<dyn AudioSink> {
        match &device {
            Some(dev) => Box::new(AlsaSink::open(dev, rate, channels, sink_gain.clone())),
            None => Box::new(NullSink),
        }
    };
    // The now-playing endpoints: the local Unix socket (on by default, so an
    // on-device display works with zero configuration) and TCP when asked
    // for. Bound before streaming starts so a bad address fails at startup
    // rather than mid-session; both serve the one publisher.
    let socket_path = tui_socket_path(
        args.tui_socket.as_deref(),
        std::env::var("XDG_RUNTIME_DIR").ok().as_deref(),
    );
    let publisher = if socket_path.is_some() || args.tui_listen.is_some() {
        Some(tui::Publisher::new(receiver.config().name.clone()))
    } else {
        None
    };
    let mut bound_socket = None;
    if let (Some((path, explicit)), Some(publisher)) = (&socket_path, &publisher) {
        match tui::bind_socket(path) {
            Ok(listener) => {
                info!("now-playing socket: {}", path.display());
                bound_socket = Some(path.clone());
                tokio::spawn(tui::serve_unix(listener, publisher.clone()));
            }
            // An explicit path that cannot be bound is a config mistake; the
            // built-in default failing (no runtime directory to bind in)
            // only costs the local display, not the receiver.
            Err(e) if *explicit => {
                eprintln!("cannot bind now-playing socket {}: {e}", path.display());
                return ExitCode::from(EXIT_CONFIG);
            }
            Err(e) => warn!(
                "no now-playing socket at {} ({e}); a local openairplay2-tui \
                 will not find this receiver automatically",
                path.display()
            ),
        }
    }
    if let (Some(addr), Some(publisher)) = (&args.tui_listen, &publisher) {
        let listener = match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => listener,
            Err(e) => {
                eprintln!("cannot listen for displays on {addr}: {e}");
                return ExitCode::from(EXIT_CONFIG);
            }
        };
        // The password is a secret: name the state, never the value.
        match &args.tui_password {
            Some(_) => info!("now-playing endpoint: ws://{addr} (password required)"),
            None => info!("now-playing endpoint: ws://{addr} (no password)"),
        }
        tokio::spawn(tui::serve(
            listener,
            publisher.clone(),
            args.tui_password.clone(),
        ));
    }

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
                    match hw_volume.as_mut() {
                        Some(hw) => hw.set(db),
                        None => gain.set(volume_to_gain(db)),
                    }
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

    let code = tokio::select! {
        result = receiver.run(sink_factory, event_tx) => {
            match result {
                Ok(()) => ExitCode::SUCCESS,
                // A bind that fails deterministically (port taken, or below
                // 1024 without privileges) is a config mistake, not a crash —
                // exit EX_CONFIG so systemd doesn't restart-loop it. Anything
                // else is unexpected and should restart.
                Err(e) => match e.kind() {
                    std::io::ErrorKind::AddrInUse => {
                        eprintln!("error: {e} — is another receiver already running?");
                        ExitCode::from(EXIT_CONFIG)
                    }
                    std::io::ErrorKind::PermissionDenied => {
                        eprintln!("error: {e} — a port below 1024 needs privileges the service does not have");
                        ExitCode::from(EXIT_CONFIG)
                    }
                    _ => {
                        eprintln!("error: {e}");
                        ExitCode::FAILURE
                    }
                }
            }
        }
        _ = shutdown_signal() => {
            info!("shutting down");
            ExitCode::SUCCESS
        }
    };
    // Best-effort: the next start replaces a leftover file anyway.
    if let Some(path) = bound_socket {
        let _ = std::fs::remove_file(path);
    }
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_strs(args: &[&str]) -> Result<Action, String> {
        parse(args.iter().map(|s| s.to_string()))
    }

    fn run_args(args: &[&str]) -> Args {
        match parse_strs(args) {
            Ok(Action::Run(args)) => *args,
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
        assert_eq!(args.password, None);
        assert_eq!(args.tui_listen, None);
        assert_eq!(args.log_level, None);
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
            "--mixer",
            "PCM",
            "--mixer-device",
            "hw:CARD=S2",
            "--password",
            "open sesame",
            "--tui-socket",
            "/run/x/tui.sock",
            "--tui-listen",
            "127.0.0.1:7392",
            "--log-level",
            "DEBUG",
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
        assert_eq!(args.mixer.as_deref(), Some("PCM"));
        assert_eq!(args.mixer_device.as_deref(), Some("hw:CARD=S2"));
        assert_eq!(args.password.as_deref(), Some("open sesame"));
        assert_eq!(args.tui_socket.as_deref(), Some("/run/x/tui.sock"));
        assert_eq!(args.tui_listen.as_deref(), Some("127.0.0.1:7392"));
        // Normalized to lowercase.
        assert_eq!(args.log_level.as_deref(), Some("debug"));
    }

    #[test]
    fn debug_is_shorthand_and_later_flag_wins() {
        assert_eq!(run_args(&["--debug"]).log_level.as_deref(), Some("debug"));
        // Later flag wins between the two spellings.
        assert_eq!(
            run_args(&["--debug", "--log-level", "warn"])
                .log_level
                .as_deref(),
            Some("warn")
        );
        assert_eq!(
            run_args(&["--log-level", "warn", "--debug"])
                .log_level
                .as_deref(),
            Some("debug")
        );
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
            ("OPENAIRPLAY2_PASSWORD", "sesame42"),
            ("OPENAIRPLAY2_AVAHI", "off"),
            ("OPENAIRPLAY2_AUDIO", "off"),
            ("OPENAIRPLAY2_ALSA_DEVICE", "hw:1"),
            ("OPENAIRPLAY2_MIXER", "Master"),
            ("OPENAIRPLAY2_MIXER_DEVICE", "default"),
            ("OPENAIRPLAY2_TUI_SOCKET", "off"),
            ("OPENAIRPLAY2_TUI_LISTEN", "0.0.0.0:7392"),
            ("OPENAIRPLAY2_TUI_PASSWORD", "sekrit"),
            ("OPENAIRPLAY2_LOG_LEVEL", "Debug"),
        ];
        let args = resolve(run_args(&[]), env_of(&env)).unwrap();
        assert_eq!(args.name.as_deref(), Some("Kitchen %h"));
        assert_eq!(args.port, Some(7100));
        assert_eq!(args.mac, Some([0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff]));
        assert_eq!(
            args.identity_file.as_deref(),
            Some(std::path::Path::new("/var/lib/x/identity"))
        );
        assert_eq!(args.password.as_deref(), Some("sesame42"));
        assert_eq!(args.avahi, Some(false));
        assert_eq!(args.mixer.as_deref(), Some("Master"));
        assert_eq!(args.mixer_device.as_deref(), Some("default"));
        assert_eq!(args.tui_socket.as_deref(), Some("off"));
        assert_eq!(args.tui_listen.as_deref(), Some("0.0.0.0:7392"));
        assert_eq!(args.tui_password.as_deref(), Some("sekrit"));
        assert_eq!(args.log_level.as_deref(), Some("debug")); // normalized
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
            ("OPENAIRPLAY2_LOG_LEVEL", "loud"),
        ];
        for (var, value) in cases {
            let env = [(var, value)];
            let err = resolve(run_args(&[]), env_of(&env)).unwrap_err();
            assert!(err.contains(var), "{err}");
            assert!(err.contains(value), "{err}");
        }
    }

    #[test]
    fn the_pincode_spellings_still_configure_the_password() {
        // The 0.4 flag is an alias.
        assert_eq!(
            run_args(&["--pincode", "1212"]).password.as_deref(),
            Some("1212")
        );
        // The 0.4 variable still protects an upgraded box…
        let env = [("OPENAIRPLAY2_PINCODE", "1212")];
        let args = resolve(run_args(&[]), env_of(&env)).unwrap();
        assert_eq!(args.password.as_deref(), Some("1212"));
        // …and the new name wins when both are set.
        let env = [
            ("OPENAIRPLAY2_PASSWORD", "new"),
            ("OPENAIRPLAY2_PINCODE", "old"),
        ];
        let args = resolve(run_args(&[]), env_of(&env)).unwrap();
        assert_eq!(args.password.as_deref(), Some("new"));
    }

    #[test]
    fn legacy_pincode_trips_only_when_set_and_nonempty() {
        assert!(legacy_pincode_notice(env_of(&[])).is_none());
        assert!(legacy_pincode_notice(env_of(&[("OPENAIRPLAY2_PINCODE", " ")])).is_none());
        let msg = legacy_pincode_notice(env_of(&[("OPENAIRPLAY2_PINCODE", "1212")])).unwrap();
        assert!(msg.contains("OPENAIRPLAY2_PASSWORD"), "{msg}");
        assert!(msg.contains("still honored"), "{msg}");
        // Never the value itself: it is a secret.
        assert!(!msg.contains("1212"), "{msg}");
    }

    #[test]
    fn legacy_args_trips_only_when_set_and_nonempty() {
        assert!(legacy_args_notice(env_of(&[])).is_none());
        assert!(legacy_args_notice(env_of(&[("OPENAIRPLAY2_ARGS", " ")])).is_none());
        let msg = legacy_args_notice(env_of(&[("OPENAIRPLAY2_ARGS", "--name X")])).unwrap();
        assert!(msg.contains("no longer read"), "{msg}");
    }

    #[test]
    fn tui_socket_path_resolution() {
        // Unset → the per-user runtime dir when there is one…
        assert_eq!(
            tui_socket_path(None, Some("/run/user/1000")),
            Some((PathBuf::from("/run/user/1000/openairplay2/tui.sock"), false))
        );
        // …the system runtime dir otherwise (a service has no XDG, and an
        // empty variable means unset).
        assert_eq!(
            tui_socket_path(None, None),
            Some((PathBuf::from("/run/openairplay2/tui.sock"), false))
        );
        assert_eq!(
            tui_socket_path(None, Some("")),
            Some((PathBuf::from("/run/openairplay2/tui.sock"), false))
        );
        // An explicit path is used verbatim and marked explicit — its bind
        // failures are config errors where the default's only warn.
        assert_eq!(
            tui_socket_path(Some("/tmp/x.sock"), Some("/run/user/1000")),
            Some((PathBuf::from("/tmp/x.sock"), true))
        );
        // And off is off.
        assert_eq!(tui_socket_path(Some("off"), Some("/run/user/1000")), None);
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
    fn default_name_carries_the_hostname() {
        // No name configured → the default template, hostname filled in.
        assert_eq!(
            display_name(None, Some("kitchen-pi".into())),
            "OpenAirPlay2 (kitchen-pi)"
        );
        // No hostname either → a bare name, never a literal "(%h)".
        assert_eq!(display_name(None, None), "OpenAirPlay2");
        // A configured name wins and still gets %h expansion.
        assert_eq!(
            display_name(Some("Studio %h".into()), Some("pi".into())),
            "Studio pi"
        );
        assert_eq!(
            display_name(Some("Living Room".into()), Some("pi".into())),
            "Living Room"
        );
    }

    #[test]
    fn help_version_and_list_devices_are_actions() {
        assert_eq!(parse_strs(&["--help"]), Ok(Action::Help));
        assert_eq!(parse_strs(&["-h"]), Ok(Action::Help));
        assert_eq!(parse_strs(&["--version"]), Ok(Action::Version));
        assert_eq!(parse_strs(&["--list-devices"]), Ok(Action::ListDevices));
        assert_eq!(
            parse_strs(&["--list-all-devices"]),
            Ok(Action::ListAllDevices)
        );
        assert_eq!(parse_strs(&["--list-mixers"]), Ok(Action::ListMixers));
    }

    #[test]
    fn mixer_config_resolves_device_and_control() {
        // No mixer → software gain, no matter the audio device.
        assert_eq!(mixer_config(&run_args(&[])).unwrap(), None);

        // Explicit control and device pass through verbatim.
        assert_eq!(
            mixer_config(&run_args(&["--mixer", "PCM", "--mixer-device", "hw:9"])).unwrap(),
            Some(("hw:9".to_string(), "PCM".to_string()))
        );

        // The default mixer device is the card of the audio device…
        assert_eq!(
            mixer_config(&run_args(&[
                "--mixer",
                "Speaker",
                "--alsa-device",
                "plughw:CARD=S2"
            ]))
            .unwrap(),
            Some(("hw:CARD=S2".to_string(), "Speaker".to_string()))
        );
        // …and `default` when the audio device names no card (or is off).
        assert_eq!(
            mixer_config(&run_args(&["--mixer", "Master"])).unwrap(),
            Some(("default".to_string(), "Master".to_string()))
        );
        assert_eq!(
            mixer_config(&run_args(&["--mixer", "Master", "--no-audio"])).unwrap(),
            Some(("default".to_string(), "Master".to_string()))
        );

        // A mixer device with no control to drive is a config mistake; the
        // message names both the flag and the variable spellings.
        let err = mixer_config(&run_args(&["--mixer-device", "hw:9"])).unwrap_err();
        assert!(err.contains("--mixer"), "{err}");
        assert!(err.contains("OPENAIRPLAY2_MIXER"), "{err}");
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
            &["--password", "x"],
            &["--pincode", "x"],
            &["--no-avahi"],
            &["--alsa-device", "x"],
            &["--no-audio"],
            &["--mixer", "x"],
            &["--mixer-device", "x"],
            &["--list-mixers"],
            &["--tui-socket", "x"],
            &["--tui-listen", "x"],
            &["--tui-password", "x"],
            &["--log-level", "info"],
            &["--debug"],
            &["--list-devices"],
            &["--list-all-devices"],
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
