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

use crate::images::{Graphics, Passthrough, Protocol};

/// Where a receiver serves its now-playing endpoint by default.
const DEFAULT_ENDPOINT: &str = "ws://127.0.0.1:7392";

/// How long the Kitty probe waits for an answer. Long enough for a round trip
/// (through tmux too, which costs microseconds), short enough that a terminal
/// which ignores the query doesn't hold up the display visibly.
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(100);

struct Args {
    endpoint: String,
    /// Forced terminal-graphics protocol, or `None` to detect one.
    images: Option<Protocol>,
    /// Where log output goes. The display owns the screen, so logs are
    /// dropped unless this names a file.
    log_file: Option<String>,
}

fn usage() -> ! {
    eprintln!(
        "usage: openairplay2-tui [--connect ws://HOST:PORT] \
         [--images auto|kitty|iterm2|none] [--log-file PATH]"
    );
    std::process::exit(2);
}

fn parse_args() -> Args {
    let mut args = Args {
        endpoint: DEFAULT_ENDPOINT.to_string(),
        images: None,
        log_file: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--connect" => args.endpoint = it.next().unwrap_or_else(|| usage()),
            "--images" => {
                let value = it.next().unwrap_or_else(|| usage());
                args.images = match value.as_str() {
                    "auto" => None,
                    other => Some(Protocol::parse(other).unwrap_or_else(|| usage())),
                };
            }
            "--log-file" => args.log_file = Some(it.next().unwrap_or_else(|| usage())),
            "-h" | "--help" => usage(),
            other => {
                eprintln!("unknown argument: {other}");
                usage();
            }
        }
    }
    args
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
    let args = parse_args();
    if let Err(e) = init_logging(&args) {
        eprintln!("{e}");
        return ExitCode::FAILURE;
    }

    // Detection has to happen before ratatui owns the terminal: the probe
    // writes a query and reads the answer itself. tmux-ness comes first
    // because the probe's own query has to be wrapped to get out of a pane —
    // and a forced --images protocol needs the wrapping just as much.
    let env = |name: &str| std::env::var(name).ok();
    let tmux = images::under_tmux(env);
    let mut images = match args.images {
        Some(protocol) => Graphics::new(protocol, tmux),
        None => Graphics::detect(env, images::probe_kitty(PROBE_TIMEOUT, tmux), tmux),
    };

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
