# Releasing openairplay2

Releasing = **publishing a GitHub Release**. That is the only event that
ships anything; nothing releases from a push or a merge.

Publishing a release runs [release.yml](../.github/workflows/release.yml),
which checks the tag against the crate version and then dispatches two
workflows that run in parallel:

- [cargo.yml](../.github/workflows/cargo.yml) — publishes
  `openairplay2-tui-protocol`, `openairplay2` and `openairplay2-receiver`, in
  that order (dependencies before dependents), to crates.io using Trusted
  Publishing (GitHub OIDC, no stored API token). Before anything uploads,
  `cargo package --workspace` must package and verify **every** publishable
  crate — a crates.io version is immutable, so a release that publishes some
  crates and then fails is the worst outcome (#65). Each publish step skips
  versions already on the index, so the job is safe to re-run after a partial
  failure.
- [debian.yml](../.github/workflows/debian.yml) — builds
  `openairplay2-receiver_X.Y.Z-1_{amd64,arm64,armhf}.deb` and
  `openairplay2-tui_X.Y.Z-1_{amd64,arm64,armhf}.deb` in parallel, in
  `debian:trixie` containers (amd64 and arm64 natively; armhf cross-compiled,
  since no 32-bit ARM runners exist), signs a provenance attestation for each,
  and attaches all six plus `SHA256SUMS` to the release.

## One-time setup (first release only)

crates.io can only configure a trusted publisher for a crate that already
exists, so the very first publish is manual:

1. Log in on crates.io with the GitHub account and create an API token
   (scope: `publish-new`), then locally:

   ```sh
   cargo login          # paste the token
   cargo publish -p openairplay2
   cargo publish -p openairplay2-receiver   # after the library is up
   ```

2. On crates.io, for **each** crate (`openairplay2` and
   `openairplay2-receiver`) → Settings → Trusted Publishing, add a GitHub
   publisher:
   - repository: `st3fan/openairplay2`
   - workflow filename: `release.yml` (the dispatcher — that is the workflow
     the OIDC token is issued for, even though the publish steps live in
     `cargo.yml`)
3. Revoke the API token — it is no longer needed.

*(Done: both crates were published by hand at 0.1.0 and 0.2.0; 0.3.0 was the
first automated release.)*

### Bootstrapping a new crate

The same constraint applies whenever the workspace grows a new publishable
crate: its very first publish must be manual, after which Trusted Publishing
takes over. Publish it by hand (`cargo login` with a `publish-new`-scoped
token, `cargo publish -p <crate>`), add the trusted publisher on crates.io
(repository `st3fan/openairplay2`, workflow `release.yml`), and revoke the
token. The workflow's skip-if-published guard means the next release passes
over the hand-published version untouched.

*(To do before the 0.4.0 release: `openairplay2-tui-protocol` — newly
publishable since #65 — needs exactly this bootstrap at 0.4.0.)*

## Releasing a version

**Model: release from trunk.** `main` carries the version, and a release is a
version-bump PR merged to `main` followed by a tag on `main` — no long-lived
release branch. The release workflow **requires the tag to equal the crate
version exactly**, prereleases included, and fails before publishing anything
otherwise, so a tag can never ship a version the manifests do not name. The bump
is therefore not optional: tag only a commit whose crate version is the tag.

### 1. Pick the version

`X.Y.Z`, semver-ish while pre-1.0: minor for features, patch for fixes. One
version line for everything — library, binary, and `.deb`. A release candidate
is a real version too: `X.Y.Z-rcN` (e.g. `0.4.1-rc1`), bumped and tagged the
same way, which publishes a **prerelease** to crates.io (not selected by default
resolution).

### 2. Version-bump PR against main

- Bump `version` in **all four** manifests — `openairplay2/Cargo.toml`,
  `openairplay2-receiver/Cargo.toml`, `openairplay2-tui/Cargo.toml`,
  `openairplay2-tui-protocol/Cargo.toml` — and all **three** in-workspace
  dependency version lines: `openairplay2 = { … version = … }` and
  `openairplay2-tui-protocol = { … version = … }` in the receiver, and
  `openairplay2-tui-protocol = { … version = … }` in the tui.
  (`openairplay2-tui` is `publish = false`, but the versions are one line for
  everything, and `cargo package --workspace` checks its manifest too.)
- Bump the `openairplay2 = "X.Y"` line in the README's Embedding section — it
  is the one version outside a manifest and it has been missed before.
- Update `notes/status.md` and the README if behavior changed.
- Run `cargo test --workspace` (this also refreshes `Cargo.lock`).
- Verify the packages build:

  ```sh
  cargo package --workspace             # every publishable crate, one command
  ./packaging/build-deb.sh              # native (this machine's architecture)
  ```

  `cargo package --workspace` (cargo ≥ 1.90; the workspace's
  `rust-version = "1.88"` is a build minimum, not a toolchain cap) packages and
  verifies **every** publishable crate, building dependents against the
  just-packaged local crates. Do not substitute per-crate
  `cargo publish --dry-run`: that verifies against the real index, so at bump
  time it always fails — the bumped library version is not on crates.io yet.
  Checking only the library, meanwhile, is exactly how
  [#65](https://github.com/st3fan/openairplay2/issues/65) went unnoticed. CI
  runs this same check on every PR, so a manifest that cannot publish fails
  long before a release; this step is the belt to that suspender.

  Building armhf locally needs the cross toolchain once
  (`./packaging/setup-build.sh cross` on an amd64 box), after which
  `./packaging/build-deb.sh armhf` works; CI does the same thing, so this is
  only for reproducing a packaging problem.

- Open the PR; Stefan merges it. The bump must be on `main` **before** the
  release: the workflow asserts the tag matches
  `openairplay2-receiver/Cargo.toml` and fails the release otherwise.

### 3. Publish the release (from main)

- **Tag:** `vX.Y.Z` — the leading `v` matters (the workflow strips it).
- **Title:** `X.Y.Z — <a few words>`.
- **Notes:** a short story of what the release *means*, followed by the
  generated changelog:

  ```sh
  gh release create vX.Y.Z --target main --title "X.Y.Z — …" \
      --notes "<story>" --generate-notes
  ```

  (`--generate-notes` appends the commit/PR list after the story, the same as
  the "Generate release notes" button in the web UI.)

### 4. Watch the workflow (~10 min)

`gh run watch`, or the Actions tab. One flaky leg does not cancel the others;
rerun failed jobs from the run page if the infrastructure hiccups.

### 5. Verify

- <https://crates.io/crates/openairplay2>,
  <https://crates.io/crates/openairplay2-receiver> and
  <https://crates.io/crates/openairplay2-tui-protocol> show the new version,
  and <https://docs.rs/openairplay2> builds and renders (a few minutes).
- The release page carries all six `.deb`s — receiver and tui, each for
  amd64, arm64 and armhf — and a `SHA256SUMS` covering them all.
- Install on a real Linux box and pair from a real Mac:

  ```sh
  base=https://github.com/st3fan/openairplay2/releases/download/vX.Y.Z
  curl -sLO $base/openairplay2-receiver_X.Y.Z-1_arm64.deb -sLO $base/SHA256SUMS
  grep arm64 SHA256SUMS | sha256sum -c -
  sudo apt-get install -y ./openairplay2-receiver_X.Y.Z-1_arm64.deb
  systemctl status openairplay2-receiver
  ```

  Upgrades restart the service automatically and keep both
  `/etc/default/openairplay2-receiver` and the pairing identity in
  `/var/lib/openairplay2` — a Mac that paired before the upgrade must not be
  asked to pair again. If the preserved options file predates the named
  variables (it still sets `OPENAIRPLAY2_ARGS`), the upgrade prints a
  migration notice in the apt output and the daemon logs an error naming the
  migration — and keeps running.
- Install the tui package the same way and point `openairplay2-tui --connect`
  at a receiver started with `--tui-listen` — it draws.
- Optional provenance check (any machine with `gh`):
  `gh attestation verify openairplay2-receiver_….deb --repo st3fan/openairplay2`.

## Testing the packages without publishing

Reach for this to try the `.deb`s on real hardware **before** committing a
version to crates.io — it touches neither crates.io nor a tag:

- Run `debian.yml` on its own from the Actions tab (workflow_dispatch, any
  branch). It builds all three architectures and leaves the `.deb`s as workflow
  artifacts; with no tag, nothing is attached or published. Locally,
  `./packaging/build-deb.sh` does the same for this machine's architecture.

There is **no way to exercise the crates.io publish without publishing**: a
GitHub Release (prerelease or not) runs the real workflow and the version check
is strict, so the tag's version — including any `-rcN` — is published for keeps
(immutable; yank-only). So a release candidate is a *deliberate* prerelease:
bump the crate version to `X.Y.Z-rcN` (§2), tag `vX.Y.Z-rcN` with `--prerelease`,
and expect `X.Y.Z-rcN` to live on crates.io. Do **not** tag `vX.Y.Z-rcN` against
a commit whose crate version is `X.Y.Z` — that is how the 0.4.0 number was once
spent on a candidate; the strict check now refuses it.

## If a release goes wrong

A published crates.io version is immutable — it cannot be replaced or deleted.
If the failure left the release **incomplete** (some crates published, some
not) but what published is *correct*, just re-run the cargo leg from the run
page: the publish steps skip versions already on the index and pick up where
the job stopped. Fix forward only when what published is **wrong**:
`cargo yank openairplay2@X.Y.Z` stops new projects resolving the bad version
(never breaking existing `Cargo.lock` users), then release a patch version.

The `.deb` side has no such constraint, but the tag pins the commit, so
re-running a failed workflow rebuilds the same broken code. Unless the failure
is pure infrastructure (a runner or docker-pull flake — those you just re-run
from the run page):

1. Diagnose: `gh run view <run-id> --log-failed`.
2. If nothing was published to crates.io yet, delete the failed release **and
   its tag**: `gh release delete vX.Y.Z --cleanup-tag --yes`.
3. Land the fix on main through a normal PR.
4. Re-create the release — same version if nothing published, otherwise the
   next patch version.

## Autopilot

A release request from Stefan is standing permission to run every step above
once the version number is agreed — except the one-time setup's token creation
and trusted-publisher configuration (crates.io UI, Stefan's), and merging the
version-bump PR, which stays Stefan's as always. Claude-run releases end the
release description with an attribution line:

> *Release done by Claude with permission of @st3fan.*
