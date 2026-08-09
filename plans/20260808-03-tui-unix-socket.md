# Now-playing over a Unix socket: the on-device display just works

Make `openairplay2-tui` work out of the box on the machine the receiver runs
on: the receiver serves the now-playing WebSocket **on a Unix domain socket
by default**, and the display **defaults to connecting to it**. No flags, no
port, no password — filesystem permissions are the access control.

## Background

Today the now-playing endpoint is opt-in TCP: `--tui-listen ADDR` binds a
port, the operator is told to keep it on loopback, and remote access wants a
password because anyone who can reach the port connects
([tui.rs](../openairplay2-receiver/src/tui.rs)). That is the right shape for
a display on *another* machine, but it makes the common case — a display on
the same box, e.g. the Pi the receiver runs on — a configuration exercise:
pick a port, set `--tui-listen`, start the display with `--connect`.

A WebSocket is just an HTTP upgrade over a byte stream; nothing about it
needs TCP. `tokio_tungstenite::accept_async` and `client_async` are generic
over any `AsyncRead + AsyncWrite` stream, so the same protocol, snapshot
logic and fan-out serve a `UnixListener` unchanged (the same pattern as
Docker's API on `/var/run/docker.sock`). A Unix socket also answers the
access question better than loopback TCP did: connecting requires filesystem
permission to the socket path, not just being a local process, and there is
no port to collide or firewall.

## Scope

- **Receiver**: serve the now-playing WebSocket on a Unix socket **by
  default**, alongside (not instead of) the existing `--tui-listen` TCP
  path. New option `--tui-socket PATH|off` (env `OPENAIRPLAY2_TUI_SOCKET`).
- **Display**: with no `--connect`, try the well-known socket paths first,
  then the legacy TCP default; `--connect` accepts a socket path as well as
  a `ws://` URL.
- **Packaging**: `RuntimeDirectory=openairplay2` in the systemd unit so the
  service has `/run/openairplay2` to bind in; options-file and README
  documentation; a NEWS.Debian entry.

### Out of scope

- **systemd socket activation** — the receiver owns its socket lifecycle,
  same as its TCP listeners.
- **Abstract-namespace sockets** — Linux-only; the display builds and is
  CI-tested on macOS, and a pathname socket works on both.
- **`ws+unix://` URL syntax** — `--connect` takes either a plain path or a
  `ws://` URL; inventing a URL scheme buys nothing here.
- **The wire protocol** — `openairplay2-tui-protocol` is untouched; this is
  transport only.

## Design

### Receiver

**Path resolution.** `--tui-socket` / `OPENAIRPLAY2_TUI_SOCKET`:

- a path → bind exactly there; failure is `EX_CONFIG` (explicit config that
  cannot be honored must fail loudly, like a bad `--alsa-device`);
- `off` → no socket;
- unset (the default) → `$XDG_RUNTIME_DIR/openairplay2/tui.sock` when
  `XDG_RUNTIME_DIR` is set (the per-user runtime dir of a manual run), else
  `/run/openairplay2/tui.sock` (what `RuntimeDirectory=` gives the service).
  A *default* path that cannot be bound (no such directory, no permission —
  e.g. a manual run on a box without the package installed and no
  `XDG_RUNTIME_DIR`) logs a warning and continues: it is a default, not a
  request, and a receiver that plays audio but has no local display socket
  beats one that refuses to start.

**Binding.** Create the parent directory if missing (`0755`), unlink a stale
socket file first (the previous process may have crashed; a *live* previous
process still holds the port-equivalent — its listener dies when the file is
unlinked, which is the standard Unix-socket trade and matches "last starter
wins"), bind, then set the socket file's mode to `0666`: any local user may
read what is playing — that is the point — and under `XDG_RUNTIME_DIR` the
`0700` parent directory already restricts it to the owner. Remove the file
on graceful shutdown, best-effort.

**Serving.** `tui::serve` and `serve_client` become generic over the stream
(`AsyncRead + AsyncWrite + Unpin`), with the peer label a `String` (an
address for TCP, "local" for the socket) — the snapshot, fan-out, resync and
ping logic are untouched. The TCP and Unix listeners run as two tasks
sharing one `Publisher`. **No password on the Unix socket**: the filesystem
is the authenticator, and mixing the Bearer gate into the local path would
only break the zero-config goal. `--tui-password` keeps gating TCP exactly
as today.

One consequence: the `Publisher` now exists (and encodes events, including
base64 of artwork) even when no display is connected, where today it exists
only under `--tui-listen`. That is a few hundred-KB base64 encodes per
track; accepted. (The broadcast send with no subscribers drops the message
immediately.)

### Display

**Default endpoints.** With no `--connect`, each reconnect round tries, in
order: `$XDG_RUNTIME_DIR/openairplay2/tui.sock`, `/run/openairplay2/tui.sock`,
then the legacy `ws://127.0.0.1:7392` — so every setup that works today
keeps working, and an on-device display finds the socket with zero flags.
The existing backoff/reconnect loop in
[client.rs](../openairplay2-tui/src/client.rs) cycles the list instead of
one URL; the status line shows which endpoint is connected.

**Explicit `--connect`.** A value starting with `ws://` or `wss://` is a URL
(today's behavior); anything else is a socket path. Over a socket the
handshake still needs a nominal URL — `ws://localhost/` with
`tokio_tungstenite::client_async` over the `UnixStream`.

### Packaging and docs

- `openairplay2-receiver.service`: `RuntimeDirectory=openairplay2` (default
  `0755`), so the service — which runs as the `openairplay2` user — has a
  place to bind.
- Options file: commented `OPENAIRPLAY2_TUI_SOCKET=` block (path, `off`, and
  the default explained).
- README: the on-device display becomes the first example (`openairplay2-tui`
  with no arguments next to a running receiver); `--tui-listen` +
  `--tui-password` demoted to the remote-display setup. NEWS.Debian entry.

## Test strategy

- **Server**: the existing tui.rs test suite runs against the generic
  `serve`; add Unix-socket variants — snapshot-then-changes and
  two-displays over a socket path in a temp dir — proving the generic path,
  plus a stale-socket-file rebind test and a mode check (`0666`).
- **Display**: unit tests for endpoint classification (`ws://` vs path) and
  the default-candidates order (pure function over the env lookup, same
  style as the receiver's `resolve`); a connect-over-socket round trip
  against a `Publisher` serving a temp-dir socket.
- **CLI**: flag/env parsing, `off`, explicit-path-fails → `EX_CONFIG`,
  HELP tripwire — alongside the existing main.rs tests.
- **macOS**: `cargo test -p openairplay2-tui` (CI) — `tokio::net::UnixStream`
  is supported there; nothing needs a `cfg`.
- **On-device acceptance (the point of the feature)**: on a box with the
  receiver running (service or manual), `openairplay2-tui` with **no
  arguments** shows the now-playing screen; as a *different* local user
  too (service case); `--tui-socket off` makes the display fall back to TCP
  or report unconnected; remote TCP with password still works.

## Acceptance criteria

- Fresh install, no configuration: receiver runs, `openairplay2-tui` on the
  same machine connects and displays — including as a different user when
  the receiver is the packaged service.
- Explicit `--tui-socket PATH` that cannot be bound exits `EX_CONFIG`; the
  unset default degrades to a warning.
- `--tui-listen`/`--tui-password` behavior is unchanged; the protocol crate
  is unchanged; macOS CI stays green.

## Phases

1. **This plan** (bottom of the stack).
2. **Receiver serves the socket** — generic `serve`, path resolution,
   binding/permissions/cleanup, CLI/env, packaging, tests.
3. **Display defaults to it** — endpoint candidates, socket connect,
   status-line label, README, tests.
