//! A full-screen now-playing display for an `openairplay2-receiver`.
//!
//! The receiver publishes what it is playing on a WebSocket — a local Unix
//! socket by default, TCP with `--tui-listen` — and this connects to it and
//! draws the current track, its cover art, and the stream's details. On the
//! machine the receiver runs on, no flags are needed: the default socket is
//! found on its own. It is a read-only view: it never sends the receiver
//! anything, and it can be started, stopped and restarted independently of
//! it.

use std::process::ExitCode;

use log::{info, warn};

mod client;
mod images;
mod tui;

use crate::client::Endpoint;
use crate::images::{Graphics, Multiplexer, Passthrough, Protocol};

/// How long the Kitty probe waits for an answer. Long enough for a round trip
/// (through tmux too, which costs microseconds), short enough that a terminal
/// which ignores the query doesn't hold up the display visibly.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);

#[derive(Debug, PartialEq)]
struct Args {
    /// `--connect`: a `ws://` URL or a socket path. `None` means look for a
    /// local receiver (see [`client::default_endpoints`]).
    endpoint: Option<String>,
    /// Forced terminal-graphics protocol, or `None` to detect one.
    images: Option<Protocol>,
    /// Where log output goes. The display owns the screen, so logs are
    /// dropped unless this names a file.
    log_file: Option<String>,
    /// Password for a receiver that requires one on its endpoint.
    password: Option<String>,
}

/// What the command line asks for; everything but `Run` prints and exits.
#[derive(Debug, PartialEq)]
enum Action {
    Run(Args),
    Help,
    Version,
}

const USAGE: &str = "usage: openairplay2-tui [options] — run with --help for the list";

const HELP: &str = "\
openairplay2-tui — full-screen now-playing display for an openairplay2 receiver

usage: openairplay2-tui [options]

options:
  --connect ENDPOINT        the receiver's now-playing endpoint: a
                            ws://HOST:PORT URL (the receiver's --tui-listen)
                            or the path of its local socket (--tui-socket).
                            By default a local receiver is found on its own:
                            $XDG_RUNTIME_DIR/openairplay2/tui.sock, then
                            /run/openairplay2/tui.sock, then
                            ws://127.0.0.1:7392
  --images auto|kitty|iterm2|none
                            terminal-graphics protocol for the cover art:
                            auto probes the terminal, none is text-only
                            (default auto)
  --log-file PATH           append logs here; the display owns the screen,
                            so without this logs are dropped
  --password PASS           password for a receiver whose endpoint requires
                            one (--tui-password on the receiver); falls back
                            to the OPENAIRPLAY2_TUI_PASSWORD variable, which
                            unlike a flag is not visible in ps
  --version                 print the version and exit
  -h, --help                print this help
";

/// Pure over its input — no process exit, no environment — so it has tests.
/// Errors are the message alone; `main` appends the usage line.
fn parse(mut it: impl Iterator<Item = String>) -> Result<Action, String> {
    let mut args = Args {
        endpoint: None,
        images: None,
        log_file: None,
        password: None,
    };
    let value = |flag: &str, it: &mut dyn Iterator<Item = String>| {
        it.next().ok_or_else(|| format!("{flag} needs a value"))
    };
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--connect" => args.endpoint = Some(value("--connect", &mut it)?),
            "--images" => {
                let v = value("--images", &mut it)?;
                args.images = match v.as_str() {
                    "auto" => None,
                    other => Some(Protocol::parse(other).ok_or_else(|| {
                        format!("--images must be auto, kitty, iterm2 or none, not \"{v}\"")
                    })?),
                };
            }
            "--log-file" => args.log_file = Some(value("--log-file", &mut it)?),
            "--password" => args.password = Some(value("--password", &mut it)?),
            "--version" => return Ok(Action::Version),
            "-h" | "--help" => return Ok(Action::Help),
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(Action::Run(args))
}

/// Logs go to a file or nowhere: stderr would shred the display.
fn init_logging(args: &Args) -> Result<(), String> {
    let mut builder =
        env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"));
    match &args.log_file {
        Some(path) => {
            let file = std::fs::File::create(path)
                .map_err(|e| format!("cannot write log file {path:?}: {e}"))?;
            builder.target(env_logger::Target::Pipe(Box::new(file)));
        }
        None => {
            builder.target(env_logger::Target::Pipe(Box::new(std::io::sink())));
        }
    }
    builder.init();
    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = match parse(std::env::args().skip(1)) {
        Ok(Action::Run(args)) => args,
        Ok(Action::Help) => {
            // Behave like a Unix tool when piped into a pager: die quietly
            // on a closed pipe instead of panicking. Only here — the client
            // keeps SIGPIPE ignored so socket writes stay io errors.
            unsafe {
                libc::signal(libc::SIGPIPE, libc::SIG_DFL);
            }
            print!("{HELP}");
            return ExitCode::SUCCESS;
        }
        Ok(Action::Version) => {
            println!("openairplay2-tui {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Err(msg) => {
            eprintln!("error: {msg}");
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    if let Err(e) = init_logging(&args) {
        eprintln!("{e}");
        return ExitCode::FAILURE;
    }

    // Detection has to happen before ratatui owns the terminal: the probe
    // writes a query and reads the answer itself. tmux-ness comes first
    // because the probe's own query has to be wrapped to get out of a pane —
    // and a forced --images protocol needs the wrapping just as much.
    let env = |name: &str| std::env::var(name).ok();
    let mux = images::multiplexer(env);
    let tmux = mux == Multiplexer::Tmux;
    let detected = match args.images {
        Some(protocol) => protocol,
        None => images::detect(env, images::probe_kitty(PROBE_TIMEOUT, tmux)),
    };
    let mut images = Graphics::in_multiplexer(detected, mux);
    if mux == Multiplexer::Screen && detected != Protocol::None {
        warn!(
            "running under GNU screen: cover art is disabled, because screen has no \
             passthrough this display can use safely"
        );
    }

    // Every wrong `allow-passthrough` value fails silently — tmux drops the
    // escape and the screen just has no picture on it — so ask, and say so.
    if tmux && images.draws() {
        let passthrough = Passthrough::query(env("TMUX_PANE"));
        if let Some(advice) = passthrough.advice() {
            warn!("{advice}");
        }
        // Nothing can get through at all: better an honest text-only layout
        // than a box reserved for a picture that will never arrive.
        if passthrough == Passthrough::Never && args.images.is_none() {
            images = Graphics::new(Protocol::None, tmux);
        }
    }

    // The endpoints to try: an explicit --connect alone, or the local
    // candidates a zero-config receiver serves by default. The label is
    // what the screen shows until (and about) a connection.
    let endpoints = match &args.endpoint {
        Some(value) => vec![Endpoint::parse(value)],
        None => client::default_endpoints(std::env::var("XDG_RUNTIME_DIR").ok().as_deref()),
    };
    let label = match &args.endpoint {
        Some(value) => value.clone(),
        None => "local receiver".to_string(),
    };

    info!(
        "connecting to {}, terminal graphics: {images}",
        endpoints
            .iter()
            .map(Endpoint::label)
            .collect::<Vec<_>>()
            .join(" or ")
    );

    // The flag wins; the variable is the ps-safe way to hand it over.
    let password = args.password.clone().or_else(|| {
        std::env::var("OPENAIRPLAY2_TUI_PASSWORD")
            .ok()
            .filter(|v| !v.is_empty())
    });
    if let Some(p) = &password {
        // Validate once, so the client can rely on it: a password must be
        // able to travel in an HTTP header.
        if format!("Bearer {p}")
            .parse::<tokio_tungstenite::tungstenite::http::HeaderValue>()
            .is_err()
        {
            eprintln!(
                "error: the password contains characters that cannot travel in an HTTP header"
            );
            return ExitCode::FAILURE;
        }
    }

    let (updates_tx, updates_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(client::run(endpoints, password, updates_tx));

    match tui::run(updates_rx, label, images).await {
        Ok(_) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("display error: {e}");
            ExitCode::FAILURE
        }
    }
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
        // No endpoint: the client searches the local candidates.
        assert_eq!(args.endpoint, None);
        assert_eq!(args.images, None);
        assert_eq!(args.log_file, None);
    }

    #[test]
    fn every_flag_parses() {
        let args = run_args(&[
            "--connect",
            "ws://10.0.0.5:7392",
            "--images",
            "kitty",
            "--log-file",
            "/tmp/tui.log",
        ]);
        assert_eq!(args.endpoint.as_deref(), Some("ws://10.0.0.5:7392"));
        assert_eq!(args.images, Some(Protocol::Kitty));
        assert_eq!(args.log_file.as_deref(), Some("/tmp/tui.log"));
    }

    #[test]
    fn connect_takes_a_socket_path_too() {
        assert_eq!(
            run_args(&["--connect", "/run/openairplay2/tui.sock"])
                .endpoint
                .as_deref(),
            Some("/run/openairplay2/tui.sock")
        );
    }

    #[test]
    fn images_accepts_the_documented_values() {
        assert_eq!(run_args(&["--images", "auto"]).images, None);
        assert_eq!(
            run_args(&["--images", "iterm2"]).images,
            Some(Protocol::ITerm2)
        );
        assert_eq!(run_args(&["--images", "none"]).images, Some(Protocol::None));
        assert!(parse_strs(&["--images", "sixel"])
            .unwrap_err()
            .contains("--images"));
    }

    #[test]
    fn help_and_version_are_actions() {
        assert_eq!(parse_strs(&["--help"]), Ok(Action::Help));
        assert_eq!(parse_strs(&["-h"]), Ok(Action::Help));
        assert_eq!(parse_strs(&["--version"]), Ok(Action::Version));
    }

    #[test]
    fn mistakes_name_the_flag() {
        assert!(parse_strs(&["--connect"])
            .unwrap_err()
            .contains("--connect"));
        assert!(parse_strs(&["--frobnicate"])
            .unwrap_err()
            .contains("--frobnicate"));
    }

    /// Every flag the parser accepts is documented in HELP — adding a flag
    /// without describing it fails here.
    #[test]
    fn help_describes_every_flag() {
        let flags: &[&[&str]] = &[
            &["--connect", "x"],
            &["--images", "none"],
            &["--log-file", "x"],
            &["--password", "x"],
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
