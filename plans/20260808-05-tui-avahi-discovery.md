# Advertise the now-playing endpoint; let the display find receivers

Make a remote `openairplay2-tui` need zero configuration too: a receiver
serving the now-playing WebSocket over TCP (`--tui-listen`) **advertises it
over Avahi**, and a display started with no endpoint that finds no local
receiver **browses for advertised ones and offers a picker**.

## Background

The on-device display already works with zero flags: the receiver serves a
Unix socket by default and the display tries the well-known paths
(plans/20260808-03-tui-unix-socket.md). The *remote* display still needs
hand-carried knowledge: the operator reads the receiver's address off its
config, types `ws://HOST:PORT`, and updates it when the box moves. But the
receiver already lives in an mDNS world — it advertises `_airplay._tcp`
through the Avahi daemon's D-Bus API ([avahi.rs](../openairplay2/src/avahi.rs))
— so the now-playing endpoint can be advertised the same way, and the
display can browse for it exactly like a sender browses for AirPlay
receivers.

## Scope

- **Receiver**: when `--tui-listen` is serving on a non-loopback address and
  Avahi advertisement is on (no `--no-avahi`), also register
  `_openairplay2._tcp` on that port, named like the AirPlay advertisement.
- **Library**: generalize `avahi::publish` to take a service type and expose
  it `#[doc(hidden)]` so the binary can register its own service.
- **Display**: with no endpoint argument, keep trying the local candidates
  first; when none answers, browse `_openairplay2._tcp` via Avahi and show a
  full-screen picker. Selecting an entry connects to it (with the existing
  reconnect loop); the zero-config local case keeps working unchanged —
  a local receiver that appears while the picker is up wins automatically.

### Out of scope

- **Discovery on macOS.** The display builds and runs on macOS, but browsing
  here goes through the Avahi daemon's D-Bus API, which macOS does not have.
  On macOS the picker reports that discovery is unavailable; explicit
  endpoints work as today. (A cross-platform pure-Rust browser — e.g.
  `mdns-sd` — could lift this later; a separate decision, filed as an issue.)
- **Advertising the Unix socket.** mDNS names network endpoints; the socket
  is found by path.
- **A loopback `--tui-listen`.** Advertising `127.0.0.1` to the network is
  noise; such an endpoint is deliberately private and is not registered.
- **Interactive password entry in the picker.** A password-protected entry
  shows as such; the password itself still comes from `--password` /
  `OPENAIRPLAY2_TUI_PASSWORD`, and a missing one surfaces as the existing
  unauthorized state.
- **The wire protocol** — `openairplay2-tui-protocol` is untouched.

## Design

### Service type

`_openairplay2._tcp`. DNS-SD service labels are limited to 15 characters
(RFC 6763 §7.2), which rules out `_openairplay2-tui._tcp`; the bare project
name fits (12) and reads as what it is: an openairplay2 receiver's
now-playing endpoint. The instance name is the receiver's display name — the
same name senders see in the AirPlay menu, so the picker and the AirPlay
menu agree. TXT records: `txtvers=1`, and `pw=1`/`pw=0` for whether
`--tui-password` gates the endpoint, so the picker can mark protected
receivers before a connection is attempted.

### Library

[avahi.rs](../openairplay2/src/avahi.rs) grows a service-type parameter
(`publish(service_name, service_type, port, txt_records)`); the AirPlay call
site passes `_airplay._tcp`. The module becomes `#[doc(hidden)] pub` —  the
same arrangement as `srp`/`tlv`/`cipher`/`server`: usable by the workspace,
not part of the documented API. No new dependencies; `zbus` is already
there, and the module keeps compiling on macOS (it only fails at runtime,
where nothing calls it).

### Receiver

A pure function decides the registration from the resolved options:
`tui_advertisement(tui_listen, avahi) -> Option<u16>` — `None` when
`--tui-listen` is unset, Avahi is off, or the bind address is loopback
(IPv4 or IPv6); otherwise the port. `main` registers after the TCP listener
binds (never advertise an endpoint that failed to exist) and holds the
`Advertisement` for the life of the process, next to the AirPlay one. A
registration failure warns and continues, exactly like the AirPlay
advertisement's failure mode: the endpoint still serves, it just is not
discoverable.

One asymmetry, accepted: binding `--tui-listen` to one specific non-loopback
address while Avahi announces all of the host's addresses can advertise an
address the listener is not on. `IF_UNSPEC`/`PROTO_UNSPEC` matches the
AirPlay registration; the wildcard bind is the documented, common case.

### Display

**Flow.** With an explicit endpoint argument nothing changes. With none:

1. The client probes the local candidates (socket paths, then legacy
   loopback TCP) as today. If one answers, the display runs as today.
2. When a full round finds no local receiver, the client reports it, and
   the display switches to the **picker**: a full-screen list (inside the
   same ratatui session — the terminal is never released and re-grabbed) of
   receivers discovered over Avahi, live: entries appear and disappear as
   `ItemNew`/`ItemRemove` arrive, each resolved to name + `host:port`, with
   a lock marker when TXT says `pw=1`. Arrow keys move, Enter connects,
   `q` quits.
3. While the picker is up, the client keeps probing the local candidates in
   the background; if a local receiver appears, it wins and the display
   connects without a keypress — the zero-config promise survives a display
   started before its receiver. A *discovered* (network) receiver is never
   auto-connected: that is a choice, not a default.
4. After a selection, the chosen endpoint behaves like an explicit one:
   the existing reconnect loop, the existing unauthorized state on 401.

**Modules.** A new `discover.rs` in `openairplay2-tui` owns the Avahi
D-Bus browsing (`ServiceBrowserNew` on `_openairplay2._tcp`, resolve on
`ItemNew`, drop on `ItemRemove`, dedupe by instance name across
interfaces/protocols), reporting `Found`/`Lost` over a channel; where Avahi
is unreachable (macOS, or no daemon) it reports that once and the picker
shows it. `zbus` (tokio features, default-features off — the library's
configuration) is added to the tui crate; the crate stays free of
`openairplay2` and ALSA.

**Wiring.** `client::run` gains the "no endpoint" mode: local rounds, then
a `NoLocalReceiver` update; selections travel back over a small channel from
the display task to the client, which then runs its normal loop on the
chosen endpoint. `tui::run` grows the picker state alongside the existing
connected/disconnected states.

### Docs

README: the remote-display section starts from discovery ("start the
receiver with `--tui-listen 0.0.0.0:7392`, run `openairplay2-tui` anywhere
on the network, pick it from the list"), with the explicit-endpoint form
kept for scripted setups. Receiver `--help` note on `--tui-listen` that a
non-loopback endpoint is advertised over mDNS unless `--no-avahi`.
NEWS.Debian entry for both packages.

## Test strategy

- **Receiver**: unit tests for `tui_advertisement` — unset, `--no-avahi`,
  loopback v4/v6, wildcard and specific addresses, port extraction, and a
  bad address (already rejected later by bind, decision is `None`).
- **Display**: picker model tests (entries appear/update/leave, selection,
  lock marker, ordering stable while navigating); classification of
  resolved services into `Endpoint::Url`; the local-first flow as a state
  test over the client's updates (no local → picker; local appears →
  auto-connect; selection → connect). D-Bus itself is not mocked — the
  browsing module is kept thin and its logic (dedupe, TXT parsing) is pure
  and tested.
- **macOS CI**: `cargo test -p openairplay2-tui` keeps passing — `zbus`
  compiles there; nothing in tests touches a live bus.
- **Hardware acceptance (skynet)**: receiver with `--tui-listen
  0.0.0.0:7392` appears in `avahi-browse -r _openairplay2._tcp` with the
  right name, port and `pw` TXT; with `--tui-password` the TXT flips;
  with `--no-avahi` or a loopback listen address, no registration. A
  display with no local receiver (`--tui-socket off` on the receiver, or
  another machine) shows the picker, lists the receiver by its AirPlay
  name, and connects on Enter; starting a local receiver while the picker
  is up connects automatically.

## Acceptance criteria

- A receiver with a non-loopback `--tui-listen` is discoverable as
  `_openairplay2._tcp`; loopback, `--no-avahi`, and no `--tui-listen`
  register nothing.
- `openairplay2-tui` with no arguments: connects to a local receiver when
  one exists (unchanged); otherwise presents discovered receivers and
  connects to the chosen one; a local receiver appearing later still wins.
- Explicit endpoints, passwords, and every current flag behave as today;
  the protocol crate is unchanged; macOS CI stays green.

## Phases

1. **This plan** (bottom of the stack).
2. **Receiver advertises** — library `publish` generalization, the
   `tui_advertisement` decision, registration in `main`, README/help,
   tests.
3. **Display discovers** — `discover.rs`, the picker, client wiring,
   README, NEWS.Debian, tests.
