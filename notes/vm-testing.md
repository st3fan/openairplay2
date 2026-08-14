# Testing the `.deb` in a VM: what works, and what doesn't

How `testing/vm/` came to boot **Debian** rather than Raspberry Pi OS, written
down because the dead end is worth not walking into twice. The procedure for
*running* the tests is [`runbooks/testing-the-deb.md`](../runbooks/testing-the-deb.md);
this is the record of why it is built the way it is.

## The problem

The `.deb` is the least-tested thing this project ships. The systemd unit, the
`openairplay2` system user, the conffile, the identity in `/var/lib`, the
dependency list — none of it is exercised by `cargo test`, and
`notes/status.md` carried "hardware validation pending" for the packages from
the day they shipped. A VM that installs the package the way a user would
closes most of that gap without a spare Pi.

## Attempt 1: Raspberry Pi OS (the obvious choice) — abandoned

`2026-06-18-raspios-trixie-arm64-lite.img` is the real target, so it was tried
first. Two ways in, both blocked:

**`-M virt` (the fast, virtio path): the Pi kernel cannot see virtio at all.**
Not built in, not a module, not shipped:

```
$ zcat kernel8.img > Image && grep -c virtio_blk Image      # 0 — nothing built in
$ cpio -t < initramfs8 | grep -c virtio                     # 0 — nothing in the initramfs
$ debugfs -R "ls .../kernel/drivers/virtio" rootfs.img
  ...: File not found by ext2_lookup                        # the package has no virtio dir
```

With no virtio-blk the kernel cannot find its root filesystem on `-M virt`, and
with no virtio-net it would have no network even if it booted.

**`-M raspi3b` (the emulate-a-Pi path): boots, then panics in USB.** QEMU 10
models the Pi 3B well enough that the image genuinely comes up — SD card
detected, ext4 root mounted, systemd starting:

```
mmcblk0: mmc0:6156 QEMU! 4.00 GiB
EXT4-fs (mmcblk0p2): mounted filesystem ... r/w
Run /sbin/init as init process
```

but Pi OS drives USB with Broadcom's out-of-tree `dwc_otg` driver, and it
NULL-derefs against QEMU's controller model:

```
dwc_otg: version 3.00a 10-AUG-2012 (platform bus)
Core Release: 2.94a
Setting default values for core params
Unable to handle kernel NULL pointer dereference at virtual address 00000000000003a8
Kernel panic - not syncing: Oops: Fatal exception in interrupt
```

Detach every USB device and it boots cleanly — but a Pi 3B's Ethernet *is*
USB-attached, so no USB means no network, and no network means no
`apt-get install`. Pi OS in QEMU is therefore usable only for offline tests,
which is not the test we want.

(Not chased further: patching the DTB to hide the USB node, `-M raspi4b`, or
grafting virtio modules into the initramfs. Each is a day of work to test a
package that is not Pi-specific.)

## What we run instead: Debian 13 (trixie) arm64

`debian-13-genericcloud-arm64.qcow2` on `-M virt`, and everything is
undramatic: UEFI (AAVMF) boots GRUB, virtio-blk finds the disk, virtio-net
gets user-mode networking, cloud-init reads a seed and installs an SSH key.
Boot to `ssh` is about 60 s under TCG.

This is a fair substitute because it is the *same distribution release the
package is built on and declares dependencies against*: trixie, glibc 2.41,
`libasound2t64`. Nothing in the `.deb` is Pi-specific — it is a plain Debian
package with a systemd unit — so what Raspberry Pi OS would add is a different
kernel and Pi hardware, neither of which the emulator provides anyway.

**What it does not cover**, and still needs a real Pi: audio actually playing
(the VM has no sound card), pairing with a real sender, and the armhf package,
which has never run anywhere.

## How the VM is built (all unprivileged)

Nothing here needs root, which is what lets the same scripts run on a GitHub
runner:

| Need | How | Why not the obvious thing |
|---|---|---|
| A disk | `qemu-img create -b <base> -F qcow2` overlay | never writes the cached base image; cleanup is one unlink |
| Seed the VM | FAT image written with `mformat`/`mcopy`, labelled `CIDATA` | cloud-init's NoCloud datasource reads a *label*, so no ISO tools and no loop mount |
| Get in | cloud-init installs an ephemeral ed25519 key generated per run | no password, nothing to leak, no `sshpass` |
| Network | QEMU user-mode networking + `hostfwd` to a free port | no bridge, no tap, no `sudo`, no collisions between parallel runs |
| Firmware | `/usr/share/AAVMF/AAVMF_CODE.fd` read-only + a copy of `AAVMF_VARS.fd` | the vars file must be writable per VM |

Packages: `qemu-system-arm qemu-utils qemu-efi-aarch64 mtools openssh-client`.

## Timings (skynet, TCG, no KVM — aarch64 on x86)

| | |
|---|---|
| Base image download (once) | ~322 MB |
| Boot to ssh | ~60 s |
| Full `vmtest` run, cold VM | ~215 s |
| Failure on a broken package | ~120 s |

Slow but bounded, and every wait has a timeout that fails with a message
rather than hanging a CI job.

## Things the first run found

- **`avahi advertisement disabled` on first install.** The daemon starts inside
  the same `apt` transaction that is still configuring `avahi-daemon` (a
  `Recommends`), so the very first start cannot advertise. It is fine after a
  reboot — which is why `deb-install.sh` reboots and *then* insists the
  advertisement worked. A user installing the package sees one scary log line
  and a receiver that is undiscoverable until something restarts it.
