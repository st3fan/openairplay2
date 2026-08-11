# Bind the session channels with the IPv6 scope id

Fixes [#168](https://github.com/st3fan/openairplay2/issues/168) — a sender that
reaches us over an **IPv6 link-local** address pairs successfully and then gets
`SETUP` 400, so it never streams.

## Background

Observed on hardware (a Mac running AirPlay/960.13.1 → a Debian VM), 2026-08-09:

```
SETUP rtsp://fe80::6e9e:e2b5:e9bb:5959/14158711893506673270 RTSP/1.0
[fe80::2cca:16ff:fe58:8765%2]:52427
SETUP phase 1: timingProtocol=PTP
SETUP failed: Invalid argument (os error 22)
-> SETUP 400
```

`GET /info`, both `/pair-setup` legs and both `/fp-setup` legs succeed, because
they all ride the socket the sender already opened. The first thing that needs a
*new* socket is the phase-1 event listener, and that is where it dies.

### Mechanism

[server.rs](../openairplay2/src/server.rs) takes the connection's local address
and immediately throws away everything but the IP:

```rust
let local_ip = stream.local_addr()?.ip();   // IpAddr — scope_id dropped here
```

[session.rs](../openairplay2/src/session.rs) stores that `IpAddr` and rebuilds a
`SocketAddr` from it at all four bind sites — the phase-1 event listener (:149),
the phase-2 control socket (:196), the realtime data socket (:206) and the
buffered-audio TCP listener (:245):

```rust
TcpListener::bind(SocketAddr::new(self.local_ip, 0))
```

`SocketAddr::new` sets `scope_id` to 0, and Linux refuses to bind an address in
`fe80::/10` without one:

| bind target | result |
|---|---|
| `fe80::6e9e:e2b5:e9bb:5959` | **`EINVAL` (os error 22)** |
| `fe80::6e9e:e2b5:e9bb:5959%enp2s0` | ok |
| `::ffff:192.168.86.133` (v4-mapped) | ok |

So the IPv4 path — and the IPv4-mapped path a dual-stack `[::]:7000` listener
produces for a v4 sender — has always worked, and only genuine IPv6 link-local
connections fail.

### Why it is easy to hit

The control socket binds `[::]:7000` ([receiver.rs](../openairplay2/src/receiver.rs)),
Avahi publishes every address of the interface including the link-local AAAA,
and Apple senders prefer IPv6. On a host whose only IPv6 address is link-local —
any LAN without an IPv6 prefix, which is most home networks and every NAT'd VM —
the sender therefore picks the one address we cannot bind. The receiver looks
fully functional right up to the moment it is asked to play.

The workaround until this lands is to take IPv6 off the wire: `use-ipv6=no` plus
`publish-aaaa-on-ipv4=no` in `/etc/avahi/avahi-daemon.conf`.

## Scope

Keep the existing design — the session channels bind the *specific* local
address the sender reached us on — and stop discarding the part of that address
that makes it bindable.

**In scope**

1. **[server.rs](../openairplay2/src/server.rs)** — hand `Session::new` the whole
   `SocketAddr` from `stream.local_addr()` instead of its `.ip()`.
2. **[session.rs](../openairplay2/src/session.rs)** — store `local_addr: SocketAddr`
   and derive bind addresses through one small helper that returns the same
   address on an ephemeral port, preserving `flowinfo` and `scope_id` for v6:

   ```rust
   fn ephemeral(addr: SocketAddr) -> SocketAddr {
       match addr {
           SocketAddr::V4(a) => SocketAddr::from((*a.ip(), 0)),
           SocketAddr::V6(a) => SocketAddrV6::new(*a.ip(), 0, a.flowinfo(), a.scope_id()).into(),
       }
   }
   ```

   Use it at all four bind sites, so no bind in the file constructs a
   `SocketAddr` from a bare `IpAddr` again.
3. **Keep `timingPeerInfo` scope-less.** The phase-1 response puts our address in
   `Addresses`/`ID` as a string; that goes on the wire to the sender, which
   resolves the scope on its own side. It stays `local_addr.ip().to_string()` —
   the two uses of the address are now deliberately different, and the code
   should say so.
4. **Name the address in the failure.** `SETUP failed: Invalid argument (os error
   22)` says nothing about *what* could not be bound — the diagnosis took a
   packet-level read of the log. Each bind annotates its error with the address
   it tried, and the `debug!` lines report the address actually bound (scope
   included) next to the port they already report.

**Out of scope**

- Binding the wildcard address instead of the specific local one. It would also
  fix the symptom, but it would put the event/data/control channels on every
  interface — worth avoiding in general and specifically while session commands
  have no auth gate ([#141](https://github.com/st3fan/openairplay2/issues/141)).
- `peer_ip` / `Event::SessionStarted`, which loses the scope the same way. That
  address is only displayed, never bound, so the loss is cosmetic; a separate
  issue if the display should show `%2`.
- Everything else IPv6-adjacent: the advertisement, `/info`, the now-playing
  socket. None of them bind a derived address.

## Test strategy

- **Unit test for the helper** in [session.rs](../openairplay2/src/session.rs):
  a v4 address keeps its IP and gets port 0; a v6 address keeps its IP,
  `flowinfo` and `scope_id` and gets port 0. Pure, so it runs in the
  macOS-portable library and CI covers it on both platforms.
- **A bind test that actually proves the fix**: discover a link-local address on
  the running host, bind the address the helper derives from it, and assert that
  the same address *without* its scope is rejected — which pins the reason, not
  just the symptom. `std` cannot enumerate interfaces, so discovery reads
  `/proc/net/if_inet6`: the test is `#[cfg(target_os = "linux")]` (it simply
  isn't compiled on macOS) and returns early on a host with no link-local
  address, so CI stays green either way.
- The existing session unit tests move to the new `Session::new` signature (the
  `local()` helper in the test module).
- **Hardware check — the acceptance test.** From a Mac to a host advertising a
  link-local AAAA (Avahi at its defaults), with `RUST_LOG=debug`: the request
  lines show a `[fe80::…%N]` peer, phase 1 and phase 2 both succeed, and audio
  plays. Then confirm the IPv4 path is untouched by pointing a sender at the v4
  address and playing again.

## Acceptance criteria

- A sender connecting over IPv6 link-local completes `SETUP` phase 1 and phase 2
  and plays audio to the end of a track.
- IPv4 and IPv4-mapped senders are unaffected.
- No bind in [session.rs](../openairplay2/src/session.rs) builds its address from
  a bare `IpAddr`.
- `timingPeerInfo` still reports a scope-less address string.
- `cargo test`, `cargo clippy --all-targets`, `cargo fmt --check` clean;
  `cargo test -p openairplay2` passes on macOS.

## Phases

One phase — the change is a helper, a field type and four call sites:

1. **`ipv6-scope`** — thread the full local address through to the binds, plus
   the unit and link-local bind tests.
