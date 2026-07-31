# Milestone 1 — Discovery & `/info`

Goal (from [`../notes.md`](../notes.md)): a Mac / iPhone lists the receiver as
an AirPlay **2** device and begins the pairing handshake against it — even
though we don't complete pairing yet (that's milestone 2). Concretely: run the
TCP control server, advertise `_airplay._tcp` with an AirPlay-2 `features`
bitmask and our Ed25519 public key, and answer `GET /info` with a plist the
sender accepts.

## Scope

In:

- **Cargo binary + lib crate** `openairplay2`, tokio-based, structured so
  integration tests can drive the server in-process (mirrors v1).
- **Device identity** (`identity.rs`): an **Ed25519** keypair (our `pk`) and a
  stable `pi` UUID, generated on first run and **persisted** to a file so the
  device identity is stable across restarts. `--identity-file` to override.
- **mDNS advertisement** (`avahi.rs`, adapted from v1's D-Bus registration):
  register `_airplay._tcp` on port 7000 with the AirPlay-2 TXT records
  (below). D-Bus via `zbus`, no `avahi-utils`.
- **Control server** (`server.rs` + `http.rs`): a TCP server on port 7000
  speaking the AirPlay HTTP/RTSP hybrid — request lines are either
  `GET /info HTTP/1.1` or `<METHOD> rtsp://… RTSP/1.0`; the response echoes
  the request's protocol token and `CSeq`. Reuses v1's line/header/body
  parsing approach, generalised for both protocols.
- **`GET /info`** → a binary plist (`application/x-apple-binary-plist`)
  containing device info + `features` + `pk` + a packed `txtAirPlay` blob,
  built from the shairport-sync template.
- **Everything else** (`POST /pair-setup`, `/pair-verify`, `/fp-setup`,
  `SETUP`, …): logged in full (method, path, headers, body length) and
  answered `501 Not Implemented`, so we can see exactly what the sender sends
  next. Pairing lands in milestone 2.
- CLI: `--name`, `--port` (default 7000), `--mac`, `--identity-file`,
  `--no-avahi`.

Out: pairing/auth, channel encryption, SETUP/audio, timing — later milestones.

## `_airplay._tcp` TXT records (from shairport-sync)

```
acl=0
deviceid=<MAC>                     e.g. 6A:F0:77:B1:90:DA
features=0x405C4A00,0x00018340     features split lo,hi (see below)
flags=0x4
model=OpenAirPlay2,1
protovers=1.1
srcvers=366.0
pi=<uuid>                          public identifier
pk=<hex of 32-byte Ed25519 pubkey>
gid=<uuid>                         group id (= pi when ungrouped)
gcgl=0
igl=0
vv=2
```

`features` is the 64-bit capability bitmask, printed as
`features=0x<lo32>,0x<hi32>`. Start from shairport-sync's known-good default
**`0x00018340405C4A00`** (transient-pairing bit 48 set, AirPlay-2 audio
enabled). This is the value that makes a modern sender offer AirPlay 2; we
pare it down later if needed.

## `GET /info` response plist

Binary plist. Static fields from shairport's `get_info_response.xml`
(`protocolVersion` `1.1`, `volumeControlType`, `playbackCapabilities`,
`receiverHDRCapability`, …), plus the fields added programmatically:

- `deviceID` = MAC string
- `features` = the bitmask (as an integer)
- `statusFlags`
- `name` = friendly name
- `model` = e.g. `OpenAirPlay2,1`
- `sourceVersion` = `366.0`
- `pi` = our UUID
- `pk` = 32-byte Ed25519 public key (data)
- `txtAirPlay` = the `_airplay._tcp` TXT records packed as length-prefixed
  strings (a single data blob)

## Module layout

```
src/main.rs      — CLI, identity load/gen, avahi, serve
src/lib.rs       — Config, module decls
src/identity.rs  — Ed25519 keypair + pi UUID, load/persist
src/mac.rs       — MAC discovery (ported from v1)
src/avahi.rs     — _airplay._tcp registration over Avahi D-Bus (adapted from v1)
src/http.rs      — HTTP/RTSP request parse + response write (generalised v1 rtsp.rs)
src/server.rs    — accept loop, dispatch, GET /info
src/info.rs      — build the /info plist + the packed txtAirPlay blob
```

## Crate choices

| Concern | Crate |
|---|---|
| Async runtime / sockets | `tokio` |
| Ed25519 identity | `ed25519-dalek` + `rand` |
| Stable UUIDs (`pi`) | `uuid` (v4) |
| Binary plist | `plist` |
| mDNS (Avahi D-Bus) | `zbus` |
| hex / base64 | `hex`, `base64` |
| logging | `log`, `env_logger` |

## Acceptance criteria

- `cargo build` / `cargo test` / `cargo clippy` clean.
- Unit tests: identity round-trips through the persistence file; the
  `txtAirPlay` packing (length-prefixed pstrings); `features` lo/hi split; the
  HTTP/RTSP request parser and protocol-echoing response writer.
- Integration test: server on an ephemeral port answers `GET /info HTTP/1.1`
  with `200 OK`, `Content-Type: application/x-apple-binary-plist`, and a body
  that parses as a plist containing `features`, `pk`, and `deviceID`; an
  unimplemented method (`POST /pair-setup`) returns `501` without dropping the
  connection.
- Manual (hardware, with the merged code + running `avahi-daemon`): a Mac or
  iPhone shows the receiver in the AirPlay list and, on selecting it, issues
  `GET /info` then `POST /pair-setup` (which we 501). The debug log shows the
  real request sequence — the input for milestone 2.

## Result

Done. 16 tests pass (14 unit + 2 integration), clippy clean, and the two
observable behaviours were verified on the live system:

- **Advertisement**: with `avahi-daemon` running, the service resolves via
  Avahi as `_airplay._tcp` on `bee.local:7000` with the full AirPlay 2 TXT
  set — `features=0x405C4A00,0x18340`, `pk=<64 hex>`, `deviceid`, `model`,
  `srcvers=366.0`, `pi`, etc.
- **`GET /info`**: a raw request returns `HTTP/1.1 200 OK`,
  `Content-Type: application/x-apple-binary-plist`, echoed `CSeq`, and a body
  that **Python's `plistlib` parses cleanly** (i.e. it's a valid Apple binary
  plist) containing `deviceID`, `features` (0x18340405C4A00), `pk` (32 bytes),
  the length-prefixed `txtAirPlay` blob, and the static capability fields.
  Unimplemented methods (`POST /pair-setup`) return `501` and the connection
  survives.

Design notes:
- Identity (`Ed25519` key + `pi` UUID) is generated on first run and persisted
  to `~/.config/openairplay2/identity`, so `pk`/`pi` are stable across
  restarts (a sender remembers the receiver by them).
- The control server speaks the HTTP/RTSP hybrid on one connection, echoing
  the request's protocol token (`HTTP/1.1` vs `RTSP/1.0`) and always sending a
  `Content-Length`, which AirPlay senders expect.
- Reused from the AirPlay 1 receiver, adapted: MAC discovery, the Avahi D-Bus
  registration (now `_airplay._tcp`), and the request/response framing.

Not verified here (needs a real sender): that a Mac/iPhone lists the receiver
and proceeds into `POST /pair-setup`. That request sequence is milestone 2's
input; the `501` + debug logging is in place to capture it.
