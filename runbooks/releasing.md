# Releasing openairplay2

Releasing = **publishing a GitHub Release**. That is the only event that
ships anything; nothing releases from a push or a merge.

Publishing a release runs [release.yml](../.github/workflows/release.yml),
which checks the tag against the crate version and then dispatches two
workflows that run in parallel:

- [cargo.yml](../.github/workflows/cargo.yml) — publishes `openairplay2` and
  then `openairplay2-receiver` to crates.io using Trusted Publishing (GitHub
  OIDC, no stored API token). The library must publish first: the binary
  depends on it and its pre-publish verification build resolves the library
  from the registry, not the workspace.
- [debian.yml](../.github/workflows/debian.yml) — builds
  `openairplay2-receiver_X.Y.Z-1_{amd64,arm64,armhf}.deb` in parallel, in
  `debian:trixie` containers (amd64 and arm64 natively; armhf cross-compiled,
  since no 32-bit ARM runners exist), signs a provenance attestation for each,
  and attaches them plus `SHA256SUMS` to the release.

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

## Releasing a version

### 1. Pick the version

`X.Y.Z`, semver-ish while pre-1.0: minor for features, patch for fixes. One
version line for everything — library, binary, and `.deb`.

### 2. Version-bump PR against main

- Bump `version` in **all four** manifests — `openairplay2/Cargo.toml`,
  `openairplay2-receiver/Cargo.toml`, `openairplay2-tui/Cargo.toml`,
  `openairplay2-tui-protocol/Cargo.toml` — and the `openairplay2 = { version
  = … }` dependency line in the receiver. (The two `tui` crates are
  `publish = false`, but the versions are one line for everything.)
- Bump the `openairplay2 = "X.Y"` line in the README's Embedding section — it
  is the one version outside a manifest and it has been missed before.
- Update `notes/status.md` and the README if behavior changed.
- Run `cargo test --workspace` (this also refreshes `Cargo.lock`).
- Verify the packages build:

  ```sh
  cargo publish --dry-run -p openairplay2
  cargo publish --dry-run -p openairplay2-receiver
  ./packaging/build-deb.sh              # native (this machine's architecture)
  ```

  **Dry-run every publishable crate, not just the library.** `cargo.yml`
  publishes the library first, and a crates.io version is immutable — so a
  receiver that fails to publish leaves the release half-shipped with nothing
  to undo. Checking only the library is exactly how
  [#65](https://github.com/st3fan/openairplay2/issues/65) went unnoticed.

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

- <https://crates.io/crates/openairplay2> shows the new version, and
  <https://docs.rs/openairplay2> builds and renders (a few minutes).
- The release page carries all three `.deb`s (amd64, arm64, armhf) and
  `SHA256SUMS`.
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
  asked to pair again.
- Optional provenance check (any machine with `gh`):
  `gh attestation verify openairplay2-receiver_….deb --repo st3fan/openairplay2`.

## Testing the workflow without releasing

- **Packaging changes:** run `debian.yml` on its own from the Actions tab
  (workflow_dispatch, any branch). It builds all three architectures and leaves the
  `.deb`s as workflow artifacts; with no tag, nothing is attached or
  published.
- **The whole thing:** tag `vX.Y.Z-rcN` with `--prerelease` (optionally
  `--target <branch>`); the version check is warn-only for prereleases. Delete
  the release and its tag afterwards:
  `gh release delete vX.Y.Z-rcN --cleanup-tag --yes`. Note that a prerelease
  still publishes to crates.io — use a version number you intend to keep, or
  expect to yank it.

## If a release goes wrong

A published crates.io version is immutable — it cannot be replaced or deleted.
Fix forward: `cargo yank openairplay2@X.Y.Z` stops new projects
resolving the bad version (never breaking existing `Cargo.lock` users), then
release a patch version.

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
