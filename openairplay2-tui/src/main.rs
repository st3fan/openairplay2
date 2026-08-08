//! A full-screen now-playing display for an `openairplay2-receiver`.
//!
//! The receiver publishes what it is playing on a WebSocket
//! (`--tui-listen`); this connects to it and draws the current track,
//! its cover art, and the stream's details. It is a read-only view: it never
//! sends the receiver anything, and it can be started, stopped and restarted
//! independently of it.

use std::process::ExitCode;

use log::{info, warn};

mod client;
mod images;
mod tui;

use crate::images::{Graphics, Multiplexer, Passthrough, Protocol};

/// Where a receiver serves its now-playing endpoint by default.
const DEFAULT_ENDPOINT: &str = "ws://127.0.0.1:7392";

/// How long the Kitty probe waits for an answer. Long enough for a round trip
/// (through tmux too, which costs microseconds), short enough that a terminal
/// which ignores the query doesn't hold up the display visibly.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);

#[derive(Debug, PartialEq)]
struct Args {
    endpoint: String,
    /// Forced terminal-graphics protocol, or `None` to detect one.
    images: Option<Protocol>,
    /// Where log output goes. The display owns the screen, so logs are
    /// dropped unless this names a file.
    log_file: Option<String>,
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
  --connect ws://HOST:PORT  the receiver's now-playing endpoint — start the
                            receiver with --tui-listen to have one
                            (default ws://127.0.0.1:7392)
  --images auto|kitty|iterm2|none
                            terminal-graphics protocol for the cover art:
                            auto probes the terminal, none is text-only
                            (default auto)
  --log-file PATH           append logs here; the display owns the screen,
                            so without this logs are dropped
  --version                 print the version and exit
  -h, --help                print this help
";

/// Pure over its input — no process exit, no environment — so it has tests.
/// Errors are the message alone; `main` appends the usage line.
fn parse(mut it: impl Iterator<Item = String>) -> Result<Action, String> {
    let mut args = Args {
        endpoint: DEFAULT_ENDPOINT.to_string(),
        images: None,
        log_file: None,
    };
    let value = |flag: &str, it: &mut dyn Iterator<Item = String>| {
        it.next().ok_or_else(|| format!("{flag} needs a value"))
    };
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--connect" => args.endpoint = value("--connect", &mut it)?,
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

    info!(
        "connecting to {}, terminal graphics: {images}",
        args.endpoint
    );

    let (updates_tx, updates_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(client::run(args.endpoint.clone(), updates_tx));

    match tui::run(updates_rx, args.endpoint, images).await {
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
        assert_eq!(args.endpoint, DEFAULT_ENDPOINT);
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
        assert_eq!(args.endpoint, "ws://10.0.0.5:7392");
        assert_eq!(args.images, Some(Protocol::Kitty));
        assert_eq!(args.log_file.as_deref(), Some("/tmp/tui.log"));
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
