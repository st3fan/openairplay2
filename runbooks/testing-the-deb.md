# Testing the `.deb` in a VM

Boots a throwaway Debian 13 (trixie) **arm64** VM, installs a package into it
the way a user would, and asserts that the result actually works. Use it before
a release, after touching anything under `packaging/`, and whenever the
dependency list or the systemd unit changes.

Nothing here needs root, and no VM survives the run.

## Prerequisites (once)

```sh
sudo apt-get install -y qemu-system-arm qemu-utils qemu-efi-aarch64 mtools openssh-client
```

`vmtest` checks for all of these and prints this line if any are missing. The
first run downloads Debian's cloud image (~322 MB) into
`~/.cache/openairplay2-vmtest` and verifies it against Debian's `SHA512SUMS`;
later runs reuse it.

## Running it

```sh
# a package you just built
./packaging/build-deb.sh arm64
./testing/vm/vmtest target/debian/openairplay2-receiver_*_arm64.deb

# or the artifact from a published release
./testing/vm/vmtest --release v0.3.0
```

Expect **~4 minutes**: about a minute to boot, the rest install plus a reboot.
Output is one line per assertion; the script exits non-zero on the first
failure and tears the VM down either way.

```
==> ssh is up after 62s
==> apt-get install ./openairplay2-receiver_0.3.0-1_arm64.deb
  ok package installed
  ok unit is enabled = enabled
  ...
==> all tests passed in 215s
```

The package must be **arm64** — the VM is aarch64 and the script refuses
anything else rather than failing confusingly later.

## What it asserts

| Assertion | Why it is in there |
|---|---|
| `apt-get install` succeeds | the command the README gives users; it is what resolves the declared dependencies |
| unit is `enabled` and `active` | "brings a systemd service that starts at boot" |
| `openairplay2` user exists, is in `audio` | without the group the daemon cannot open `/dev/snd` on real hardware |
| `/var/lib/openairplay2` owned by it | `StateDirectory=` |
| an identity file appears | proof the daemon *ran*, not just that the unit started |
| `/etc/default/…` is a conffile | otherwise local edits are lost on upgrade |
| `GET /info` returns 200 + `bplist00` | the first thing a real sender asks for |
| no crash in the journal | |
| still `active` after a reboot | the only honest test of "starts at boot" |
| avahi advertisement works on a clean boot | a silent failure here means no sender ever discovers the receiver |
| identity survives `apt-get remove` | the upgrade promise in [releasing.md](releasing.md): no re-pairing |

## When something fails

Keep the VM and go in:

```sh
./testing/vm/vmtest --keep <deb>
# prints: ssh -i /tmp/openairplay2-vmtest.XXXX/id_vm -p <port> oap@127.0.0.1
```

The work directory holds everything from the run:

| File | What it is |
|---|---|
| `serial.log` | the guest's console — where a boot failure or kernel message lands |
| `install.log` | full `apt-get install` output (the failure message prints its path) |
| `qemu.log` | QEMU's own stderr; look here if the VM died rather than misbehaved |

The user is `oap`, with passwordless `sudo`. Remember to `pkill -x
qemu-system-aarch64` when finished with a `--keep` VM.

## Adding a test

Drop a `.sh` file in `testing/vm/tests/`. It is sourced with a booted VM and
these in scope:

- `ssh_vm <cmd…>` / `scp_vm <local> <remote>` — run/copy in the guest
- `assert <what> <expected> <actual>` and `assert_contains <what> <needle> <haystack>`
- `info` / `pass` / `fail` — `fail` aborts the run
- `$DEB` (host path to the package), `$WORK` (the run's directory)
- `reboot_vm` — reboots and waits for ssh to come back

## Limits

The VM has **no sound card and no sender**, so it proves the package installs
and the daemon starts and serves — never that audio plays or that pairing
works. Those still need a real Pi and a real Mac, and remain part of a
milestone's acceptance criteria.

**armhf is not covered.** The VM is aarch64; the cross-compiled armhf package
has never been run anywhere. See [`notes/vm-testing.md`](../notes/vm-testing.md)
for why Raspberry Pi OS itself is not the guest, and what it would take.

## CI

Not wired up yet. The scripts are deliberately root-free and take the package
path as an argument, so the workflow step is `apt-get install` of the five
packages above plus `./testing/vm/vmtest <artifact>` against what `debian.yml`
just built.
