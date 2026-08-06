# Cover art through tmux

- **Date:** 2026-08-05
- **Status:** proposed
- **Scope:** `openairplay2-tui` only — [`images.rs`](../openairplay2-tui/src/images.rs),
  its three call sites in [`tui.rs`](../openairplay2-tui/src/tui.rs) and
  [`main.rs`](../openairplay2-tui/src/main.rs), and the README's display
  section. Nothing in the library or the receiver changes.
- **Issue:** [#58](https://github.com/st3fan/openairplay2/issues/58) — no cover
  art inside tmux.

## Background

`openairplay2-tui` draws cover art with the Kitty graphics protocol or iTerm2
inline images. Run inside tmux the text layout appears and the artwork does
not, for two independent reasons:

1. **Detection is blind through tmux.** `TERM` becomes `tmux-256color` /
   `screen-256color`, so the `TERM` tests in `images::detect` no longer match,
   and tmux 3.2+ additionally overwrites `TERM_PROGRAM` with `tmux`. What is
   left is whatever leaked into the tmux server's environment
   (`KITTY_WINDOW_ID`, `GHOSTTY_*`, `LC_TERMINAL`, `KONSOLE_VERSION`) — present
   if the server was started from that terminal, absent otherwise. The Kitty
   probe, which would settle it outright, cannot get an answer either: the
   query is swallowed on the way out.
2. **The escapes are not wrapped.** tmux only forwards an escape sequence that
   is wrapped in its DCS passthrough envelope, and there is no `tmux` handling
   anywhere in `images.rs`. This is why `--images kitty` cannot work around the
   first problem — forcing the protocol still emits bytes tmux eats.

Both halves are ours. The user's half is `set -g allow-passthrough on`: tmux
3.3+ gates passthrough behind that option and it defaults to off. Necessary,
not sufficient — hence this plan.

### What skynet actually looks like

The development workstation is the reported case, which makes it the test rig
as well as the target — Debian trixie, tmux 3.5a, Ghostty 1.3.1 outside. Inside
a pane:

```
TMUX=/tmp/tmux-1000/default,…    TERM=xterm-256color    TERM_PROGRAM=tmux
GHOSTTY_RESOURCES_DIR=/usr/share/ghostty    GHOSTTY_BIN_DIR=/usr/bin
$ tmux show -gv allow-passthrough
off
```

Three things to take from that. `TERM` is **not** `tmux-*` here (tmux's
`default-terminal` is set to `xterm-256color`), so `$TMUX` is the signal that
actually fires and the `TERM` prefix test is only a backstop — worth having,
never load-bearing. The `GHOSTTY_*` variables *did* leak in, so `detect`
already returns `Kitty` and the escapes already go out: this is the empty-box
half of the issue, not the no-box half. And `allow-passthrough` is off, so the
first thing to verify after the change is that turning it on is what flips the
artwork on.

## The envelope

```
ESC P tmux ; <payload, every inner ESC doubled> ESC \
```

So the Kitty delete escape `\e_Ga=d,d=I,i=7332\e\\` goes out as
`\ePtmux;\e\e_Ga=d,d=I,i=7332\e\e\\\e\\`. One pure function,
`passthrough(inner: &[u8]) -> Vec<u8>`, does this, and everything else is a
decision about *what* to put through it.

**Not the cursor move.** `cursor_to` emits an absolute `ESC [ row ; col H`, and
those coordinates are the *pane's*, which only tmux knows how to translate.
Passed through it would address the outer terminal's screen and put the artwork
in the wrong place on any pane that isn't full-screen at the top-left. The
cursor move stays a normal escape that tmux interprets; only the graphics
escape that follows is wrapped.

**Per escape, not per drawing.** Kitty payloads are already chunked at 4096
bytes, and each chunk is its own `ESC _G … ESC \`; each gets its own envelope,
which also keeps every passthrough well under any DCS buffer limit. The iTerm2
drawing is a single OSC 1337 carrying the whole base64 image, so it becomes one
large passthrough — fine for the artwork senders actually send (tens of KB),
and a limitation worth knowing if a sender ever ships something enormous.

## The probe

`probe_kitty` writes the graphics query followed by a Device Attributes request
whose answer marks the end of the reply. Under tmux **both** go in one envelope,
in that order.

The alternative — wrapping only the graphics query and letting tmux answer DA1
itself — races: tmux would reply immediately, `ends_with_device_attributes`
would end the wait, and the outer terminal's `OK` arriving a moment later would
be missed. Answered by the same terminal, in order, the DA1 reply still means
"nothing more is coming".

This does assume tmux forwards the outer terminal's replies to the pane, which
it does for device attributes it did not itself request. If it doesn't, or if
`allow-passthrough` is off, the probe reads nothing, returns `None`, and
detection falls back to the environment exactly as it does today. The 100 ms
timeout is unchanged: the extra hop through tmux is microseconds, and a wrong
guess here costs a startup pause on every terminal that stays silent.

## Failing closed, and where we stop

`detect` fails closed on purpose — escapes at a terminal that can't read them
spray base64 over the screen. Under tmux the failure mode is gentler: tmux
either forwards the sequence or eats it, so the worst case is a reserved
artwork box with nothing in it, never garbage.

That is the case for keeping the environment fallback under tmux rather than
demanding a positive probe. Demanding one would be stricter, but it would also
switch off iTerm2 under tmux entirely (there is no iTerm2 probe) even where
passthrough is on and images would work. So: probe first, environment second,
and when neither can see out of tmux the user has `--images kitty|iterm2` —
which *now* means something, because the escapes are wrapped.

`detect` itself needs no new rules. tmux's own `TERM` and `TERM_PROGRAM` values
match nothing in the table, and an inherited `TERM_PROGRAM=ghostty` from an
older tmux is still true. The startup log line grows the tmux fact, so
`--log-file` answers "is it wrapping?" as well as "which protocol?".

## Module shape

`Protocol` and tmux-ness always travel together, and passing a bare `bool`
alongside a protocol to three call sites invites getting one of them wrong. So
a `Copy` struct owns both:

```rust
pub struct Graphics { protocol: Protocol, tmux: bool }

impl Graphics {
    pub fn new(protocol: Protocol, tmux: bool) -> Graphics;
    pub fn detect(env: impl Fn(&str) -> Option<String>, probe: Option<bool>, tmux: bool) -> Graphics;
    pub fn draws(self) -> bool;                     // protocol != None
    pub fn draw(self, content_type: &str, image: &[u8], placement: Placement) -> Option<Vec<u8>>;
    pub fn clear(self) -> Option<Vec<u8>>;
}

pub fn under_tmux(env: impl Fn(&str) -> Option<String>) -> bool;
pub fn probe_kitty(timeout: Duration, tmux: bool) -> Option<bool>;
```

`detect`, `draw` and `clear` stay as they are underneath — pure functions over
bytes, with the whole terminal table already tested against them — and
`Graphics` is the thin layer that decides whether the result gets an envelope.
`tui.rs` swaps `Protocol` for `Graphics` in `NowPlaying`, `TerminalGuard`,
`draw_artwork` and `run`/`event_loop`; `state.images != Protocol::None` becomes
`state.images.draws()`.

`main.rs` reads tmux-ness first, because the probe needs it:

```rust
let env = |name: &str| std::env::var(name).ok();
let tmux = images::under_tmux(env);
let images = match args.images {
    Some(protocol) => Graphics::new(protocol, tmux),   // --images still gets wrapping
    None => Graphics::detect(env, images::probe_kitty(PROBE_TIMEOUT, tmux), tmux),
};
```

`under_tmux` is `$TMUX` non-empty, or `TERM` starting with `tmux` or `screen`.
The `screen` prefix deliberately catches GNU screen too, whose passthrough
envelope is *not* tmux's; there the wrapped sequence is discarded and the user
gets no artwork instead of a screenful of base64, which is the right way to be
wrong.

## Tests

Everything here is a byte sequence, and a typo in one costs a hardware round
trip to find. So the wrapped forms are asserted exactly, the way the existing
escape-builder tests do:

- `under_tmux` over a table: `TMUX` set; `TERM=tmux-256color`; `TERM=screen`;
  and the negatives (`xterm-kitty`, empty `TMUX`, nothing at all).
- `passthrough` doubles *every* inner `ESC` and terminates with a single one —
  checked on the Kitty delete escape, which has one at each end.
- Kitty draw under tmux: the cursor move is outside the envelope and unchanged;
  each chunk of a multi-chunk payload is separately wrapped; unwrapping every
  envelope reassembles exactly what the non-tmux path emits. That last one is
  the real assertion — it makes the tmux path a transformation of the known-good
  path rather than a second implementation of it.
- iTerm2 draw under tmux: one envelope around the whole OSC, BEL and all.
- `clear` under tmux: the exact expected byte string, spelled out.
- The probe query builder, factored out of `probe_kitty` so it can be tested:
  plain when not under tmux; under tmux, one envelope containing both the
  graphics query and the DA1 request, in that order.
- `Graphics::draws()` and that `Protocol::None` emits nothing under tmux either.

## README

The display section gains a short tmux note: `set -g allow-passthrough on` is
required (with the `~/.tmux.conf` line), `--log-file` reports the detected
protocol and whether it is wrapping, and `--images kitty|iterm2` forces the
protocol when nothing about the outer terminal survives into the pane.

It also gains the honest caveat: tmux does not track images, so a pane redraw
or a scroll can leave a stale image behind. Our screen is a single in-place
frame that deletes its image on exit, which is the friendly case, but the
limitation is real.

## Found while testing: the image outlives the window

*Added after phase 1 was on screen.* Draw the artwork, switch tmux windows, and
the image stays where it was — floating over whatever window you switched to.
The plan called this "a pane redraw or a scroll can leave a stale image behind"
and filed it under caveats. That was too generous: it happens on every window
switch, which is not a caveat but the display scribbling on other people's
windows.

tmux does not track images, so nobody deletes ours. But tmux does say when the
pane stops being the visible one: with `focus-events on` it sends the pane a
FocusOut as the active pane or window changes. So the display can delete its
own image the moment it stops being looked at, and re-transmit it on FocusIn.

The catch is the same option as before, one value further along:

```
allow-passthrough [on | off | all]
    If set to on, passthrough sequences will be allowed only if the pane is
    visible.  If set to all, they will be allowed even if the pane is invisible.
```

The delete is sent *after* we stop being visible, so under `on` tmux drops the
very sequence that would clean up. **`all` is therefore the setting the README
should recommend**, with `on` still enough to draw.

`all` also makes the opposite mistake possible — a background display
transmitting artwork over the window you are actually looking at, every time
the track changes. So focus is a gate, not just a trigger: while unfocused the
display transmits **nothing**, and the pending artwork is drawn when focus
returns. That is what makes `all` safe to recommend.

### Phase 2

- Enable focus reporting under tmux only (`EnableFocusChange` on the way in,
  `DisableFocusChange` on the way out). Outside tmux, focus loss just means
  another window is in front and there is nothing to clean up; leaving the
  terminal untouched keeps that path exactly as it is today.
- FocusLost → send the delete, forget what is drawn, and stop drawing.
  FocusGained → start drawing again; the normal redraw re-transmits.
- `draw_artwork` takes a writer instead of reaching for stdout, so the byte
  stream can be asserted in tests: the delete goes out on focus loss, nothing
  at all goes out while unfocused (the `all` footgun), and the image comes back
  on focus gained.
- README: recommend `allow-passthrough all` and `focus-events on`, and say what
  each buys.
- Ask tmux what `allow-passthrough` actually is
  (`display-message -p '#{allow-passthrough}'`, which resolves pane, window and
  global settings the way tmux does) and log what that means. Every wrong value
  fails identically and silently — no picture, no complaint — and `on` is the
  value anyone who followed the first draft of this plan already has.

The complete fix for this whole class — stale images after a scroll, a redraw,
or copy-mode, with no tmux options at all — is Kitty's **unicode placeholder**
placement, where the image becomes text cells tmux itself redraws. That is a
different way of drawing, Kitty-only, and it belongs in its own issue rather
than bolted onto this one.

## Out of scope

- **GNU screen** and its own passthrough envelope. Detected only so that it
  fails quietly.
- **Sixel**, which is what a terminal without either protocol would need.
- **Repairing stale images** after a scroll, a pane redraw or copy-mode. Phase 2
  handles the window switch, which is the case that happens constantly; the
  rest needs placeholder placement.
- **Interrogating the tmux server's environment** (`tmux show-environment -g`)
  to recover a `KITTY_WINDOW_ID` the pane never inherited. Shelling out to
  tmux for a hint the probe already provides is not worth it.

## Acceptance criteria

1. `cargo test`, `cargo clippy --all-targets`, `cargo fmt --check` clean; the
   tui crate still builds on macOS (CI enforces it).
2. On skynet, in tmux inside Ghostty, with `allow-passthrough on`: cover art
   appears, changes with the track, and is gone after `q`. Toggling
   `allow-passthrough` off and on is what turns the artwork off and on — that
   is the proof the envelope is what carried it.
3. The same with `--images kitty` forced, and with the probe the only signal
   (`env -u GHOSTTY_RESOURCES_DIR -u GHOSTTY_BIN_DIR`, so nothing about the
   outer terminal survives into the pane) — `--log-file` shows the protocol and
   the tmux fact.
4. With `allow-passthrough` off: no garbage on screen, and `--images none`
   still gives a clean text-only display.
5. Outside tmux, every terminal that drew artwork before draws it unchanged.
6. With `allow-passthrough all`, switching tmux windows away from the display
   leaves no image behind, and switching back brings it straight up. With `on`
   it still draws, and the log says why the image stayed behind.

Verified for 6 against a throwaway tmux server (its own socket, a client
attached through `script(1)` so everything tmux writes is recorded): with
`allow-passthrough on` the pane *is* told it lost focus and the delete escape
is swallowed; with `all` the same delete comes out the other side. That
separates the two halves — tmux's focus reporting works, the option is what
decides whether the cleanup lands — without needing anyone to watch a screen.

## Phases

1. The `Graphics`/`passthrough` work, its tests, and the README note together —
   they are one change and splitting them would leave a commit that wraps
   escapes nobody documents.
2. Focus as a gate: delete the image when the pane stops being the visible one,
   transmit nothing while it isn't, redraw when it is again. Found by testing
   phase 1 on screen; see above.
