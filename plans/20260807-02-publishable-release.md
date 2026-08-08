# Make the release publishable again

Fixes [#65](https://github.com/st3fan/openairplay2/issues/65): publish
`openairplay2-tui-protocol` to crates.io, make the publish pipeline idempotent,
and give the runbook a pre-flight check that actually works at bump time.

## Background

Releasing 0.4.0 today would half-publish. `openairplay2-receiver` depends on
`openairplay2-tui-protocol` by path with no version (arrived with `--tui-listen`,
PR #54, after the v0.3.0 tag), so its `cargo publish` fails the manifest check —
and `cargo.yml` publishes the library first, so the release would immutably put
`openairplay2 0.4.0` on crates.io and then fail on the binary, which can only be
yanked, not undone.

Decision already taken (issue #65, roadmap): **publish the protocol crate**
rather than cut the dependency. Its JSON is already treated as a published wire
format — every message's exact serialization is pinned by tests — so publishing
it makes the existing promise official.

Two findings from scouting this plan shape the design:

1. **The runbook's current pre-flight is itself broken at bump time.**
   `cargo publish --dry-run -p openairplay2-receiver` verifies against the
   *real* crates.io index, so during a version-bump PR it fails on every minor
   bump regardless of #65:

   ```
   failed to select a version for the requirement `openairplay2 = "^0.4.0"`
   candidate versions found which didn't match: 0.3.0, 0.2.0, 0.1.0
   ```

   Workspace-aware packaging (cargo ≥ 1.90) fixes this: it verifies dependent
   crates against a local temporary registry containing the just-packaged
   workspace crates. Verified here with cargo 1.97:
   `cargo publish --dry-run --workspace` packages all three publishable crates
   (skipping `openairplay2-tui`, which is `publish = false`), builds the
   receiver against the tmp-registry copies, and stops before upload.

2. **Trusted Publishing cannot create a crate.** crates.io only lets you
   configure a trusted publisher for a crate that already exists (the same
   constraint the runbook's one-time setup records for 0.1.0), so the first
   publish of `openairplay2-tui-protocol` must be manual, with a token. That
   first publish happens *before* the 0.4.0 release — which means the release
   workflow will meet an already-published protocol crate and must skip it
   rather than fail. `cargo publish --workspace` is not trustworthy for that
   (its behavior on already-published members is at best undocumented —
   [cargo#14789](https://github.com/rust-lang/cargo/issues/14789)), so the real
   publishes stay per-crate, each guarded by an index check.

## Scope

- `openairplay2-tui-protocol` becomes publishable and gets published.
- `openairplay2-receiver`'s dependency on it gains a `version`.
- `cargo.yml` publishes three crates, in dependency order, idempotently.
- `runbooks/releasing.md`: a working pre-flight, the new crate in the bump
  checklist, and the one-time bootstrap for the new crate.

**Out of scope:**

- Publishing `openairplay2-tui` to crates.io. It stays `publish = false`; the
  roadmap's "Package `openairplay2-tui`" item is about the `.deb`, and whether
  the binary should also be `cargo install`-able is that item's question, not
  this one's.
- `debian.yml` and the `.deb`s — untouched by any of this.
- Version bumps — those happen at release time per the runbook, not here.

## Changes

### Manifests

- `openairplay2-tui-protocol/Cargo.toml`: drop `publish = false` and replace
  its "not on crates.io yet" comment — the wire format is fixture-pinned and
  treated as public, so publishing is now the deliberate choice the old comment
  was waiting on.
- `openairplay2-receiver/Cargo.toml`:
  `openairplay2-tui-protocol = { version = "0.4.0", path = "…" }` — mirroring
  how the `openairplay2` dependency is declared.
- `openairplay2-tui/Cargo.toml`: its `publish = false` comment says "see
  openairplay2-tui-protocol", whose rationale this plan deletes; give it its own
  one-line reason (distributed as a package, not a library; crates.io presence
  is the packaging item's call). **Found during implementation:** its protocol
  dependency needs `version` alongside `path` too — unlike
  `publish --workspace`, `cargo package --workspace` does *not* skip
  `publish = false` members, so the gate packages and verifies all **four**
  crates. Broader coverage than planned, and it means the tui is publish-ready
  if the packaging item ever wants that.

### `cargo.yml`

Two-stage publish job, replacing the current two hand-ordered steps:

1. **Gate: `cargo package --workspace`** before anything uploads. Packaging is
   where the manifest check that #65 tripped on lives, and the workspace flavor
   verifies every publishable crate against the tmp-registry — so any crate
   that cannot publish fails the job while crates.io is still untouched. This
   is the workflow-side fix for the half-publish failure mode, not just a
   runbook note. (`cargo package` rather than `publish --dry-run`: the dry-run
   errors on versions that already exist on the index, which is exactly the
   situation re-runs are in.)
2. **Per-crate publishes in dependency order** — `openairplay2-tui-protocol`,
   `openairplay2`, `openairplay2-receiver` — each step first consulting the
   sparse index (`https://index.crates.io/…`) and **skipping if this version is
   already published**. A 404 (brand-new crate) counts as not published. This
   makes the job idempotent: re-running after a partial failure skips what
   landed and publishes the rest — today a re-run fails on the already-published
   library and the runbook prescribes fixing forward instead. It is also what
   lets the manually bootstrapped protocol crate (below) pass through the 0.4.0
   release untouched.

Publish steps may pass `--no-verify`, since the gate already verified every
crate from its packaged form; decided at implementation by what keeps the job
simplest. `cargo publish` continues to wait for each dependency to be available
in the index before verifying dependents, as the existing workflow comment
notes.

### `runbooks/releasing.md`

- **Pre-flight**: replace the two `cargo publish --dry-run -p …` commands with
  `cargo package --workspace` (needs cargo ≥ 1.90; the workspace's
  `rust-version = "1.88"` is a *minimum* for building and does not cap the
  toolchain used for releasing). Note why the per-crate dry-run cannot work at
  bump time.
- **Bump checklist**: the receiver now has *two* dependency version lines to
  bump — `openairplay2` and `openairplay2-tui-protocol`. Adjust the "the two
  tui crates are publish = false" parenthetical (only one is now).
- **One-time setup**: a subsection for bootstrapping a *new* crate, instanced
  for `openairplay2-tui-protocol`: after the implementation PR merges and
  before the 0.4.0 release, publish it manually with a `publish-new`-scoped
  token, configure Trusted Publishing for it (repository `st3fan/openairplay2`,
  workflow `release.yml`), revoke the token. The crates.io UI steps and token
  are Stefan's, per the existing autopilot boundary.
- **Failure procedure**: note that the cargo leg is now safe to re-run after a
  partial publish (the guards skip what already landed), narrowing "fix
  forward" to the case where what landed is *wrong*, not merely incomplete.

## Sequencing

1. Plan PR (this document) — Stefan approves.
2. Implementation PR (one phase, stacked on the plan): manifests + `cargo.yml`
   + runbook. Closes #65 on merge.
3. Stefan, one-time, before the 0.4.0 release: manual first publish of
   `openairplay2-tui-protocol@0.4.0` + trusted-publisher setup. The brief state
   where the protocol crate is at 0.4.0 while the others are at 0.3.0 is
   harmless — nothing published depends on it yet.
4. The 0.4.0 release exercises the whole path: gate, skip (protocol), publish
   (library, receiver).

## Test strategy

- **Red → green on the manifest fix**: `cargo package --workspace` fails on
  `main` today with exactly the #65 error and must pass on the implementation
  branch. This is the check CI can run without publishing anything, so it also
  joins `ci.yml`'s Linux job — the class of bug #65 belongs to (a manifest
  that cannot publish) currently has no test at all, and stays invisible until
  a release.
- **Index-guard logic**: exercised locally against known cases —
  `openairplay2@0.3.0` (published → skip), `openairplay2@0.4.0` (absent →
  publish), `openairplay2-tui-protocol@0.4.0` (crate absent entirely, 404 →
  publish).
- **The real publish path** can only truly run at release time; the 0.4.0
  release is the final validation, per the runbook's existing stance on
  testing-by-prerelease.

## Acceptance criteria

- `cargo package --workspace` passes on the implementation branch and runs in
  CI on every PR.
- `cargo publish --dry-run -p openairplay2-tui-protocol` is no longer refused
  by `publish = false`.
- `cargo.yml` cannot upload anything unless every publishable crate packages
  and verifies; its publish steps skip already-published versions, so re-runs
  and the bootstrapped protocol crate are both safe.
- `runbooks/releasing.md` pre-flight works during a version-bump PR (the
  current one demonstrably does not).
- Issue #65 closes with the implementation PR.
- The 0.4.0 release lands all three crates at 0.4.0 on crates.io (validated at
  release time, not in this stack).
