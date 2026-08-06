# Boot a throwaway Debian arm64 VM and talk to it. Sourced by ../vmtest.
#
# Everything here runs unprivileged: the disk is a qcow2 overlay, the
# cloud-init seed is a FAT image written with mtools, and QEMU's user-mode
# networking needs no bridge or tap. No sudo, anywhere — which is what lets the
# same scripts run on a GitHub runner.

IMAGE_URL_BASE=${IMAGE_URL_BASE:-https://cloud.debian.org/images/cloud/trixie/latest}
IMAGE_NAME=${IMAGE_NAME:-debian-13-genericcloud-arm64.qcow2}
CACHE_DIR=${CACHE_DIR:-$HOME/.cache/openairplay2-vmtest}

# The guest is emulated aarch64 on x86 — TCG, no KVM — so it is slow but
# predictable. These are generous enough for a cold GitHub runner.
VM_MEMORY=${VM_MEMORY:-2G}
VM_CPUS=${VM_CPUS:-4}
BOOT_TIMEOUT=${BOOT_TIMEOUT:-300}

UEFI_CODE=/usr/share/AAVMF/AAVMF_CODE.fd
UEFI_VARS=/usr/share/AAVMF/AAVMF_VARS.fd

require_tools() {
    local missing=()
    for t in qemu-system-aarch64 qemu-img mformat mcopy ssh scp ssh-keygen curl sha512sum; do
        command -v "$t" >/dev/null || missing+=("$t")
    done
    [ -r "$UEFI_CODE" ] || missing+=("$UEFI_CODE (qemu-efi-aarch64)")
    if [ ${#missing[@]} -gt 0 ]; then
        fail "missing: ${missing[*]}
install with: sudo apt-get install -y qemu-system-arm qemu-utils qemu-efi-aarch64 mtools openssh-client"
    fi
}

# Download the base image once and verify it against Debian's published
# SHA512SUMS. A corrupt cached image would otherwise fail as a mystery boot
# hang half an hour later.
fetch_image() {
    mkdir -p "$CACHE_DIR"
    local img=$CACHE_DIR/$IMAGE_NAME
    if [ ! -f "$img" ]; then
        info "downloading $IMAGE_NAME (~320 MB, once)"
        curl -fsSL -o "$img.part" "$IMAGE_URL_BASE/$IMAGE_NAME"
        mv "$img.part" "$img"
    fi
    curl -fsSL -o "$CACHE_DIR/SHA512SUMS" "$IMAGE_URL_BASE/SHA512SUMS"
    ( cd "$CACHE_DIR" && sha512sum --ignore-missing -c SHA512SUMS >/dev/null ) \
        || fail "$IMAGE_NAME failed checksum verification (delete it and retry)"
    info "base image verified: $img"
}

# A qcow2 overlay, so a run never writes to the cached base image and cleanup
# is a single unlink.
prepare_disk() {
    qemu-img create -f qcow2 -b "$CACHE_DIR/$IMAGE_NAME" -F qcow2 \
        "$WORK/disk.qcow2" "${VM_DISK_SIZE:-8G}" >/dev/null
    cp "$UEFI_VARS" "$WORK/vars.fd"
}

# cloud-init's NoCloud datasource reads a filesystem labelled CIDATA. mtools
# writes the FAT image without a loop mount, so this needs no privileges.
prepare_seed() {
    ssh-keygen -q -t ed25519 -N '' -f "$WORK/id_vm" -C openairplay2-vmtest
    cat > "$WORK/meta-data" <<-EOF
	instance-id: openairplay2-vmtest
	local-hostname: oap-vmtest
	EOF
    cat > "$WORK/user-data" <<-EOF
	#cloud-config
	users:
	  - name: $VM_USER
	    sudo: 'ALL=(ALL) NOPASSWD:ALL'
	    shell: /bin/bash
	    ssh_authorized_keys:
	      - $(cat "$WORK/id_vm.pub")
	EOF
    truncate -s 8M "$WORK/seed.img"
    mformat -i "$WORK/seed.img" -v CIDATA ::
    mcopy -i "$WORK/seed.img" "$WORK/user-data" "$WORK/meta-data" ::
}

# Any free port, so parallel runs and busy CI machines don't collide.
pick_port() {
    python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1]); s.close()'
}

boot_vm() {
    SSH_PORT=$(pick_port)
    info "booting (ssh forwarded to 127.0.0.1:$SSH_PORT, console → $WORK/serial.log)"
    qemu-system-aarch64 \
        -M virt -cpu max -smp "$VM_CPUS" -m "$VM_MEMORY" \
        -drive "if=pflash,format=raw,unit=0,file=$UEFI_CODE,readonly=on" \
        -drive "if=pflash,format=raw,unit=1,file=$WORK/vars.fd" \
        -drive "file=$WORK/disk.qcow2,format=qcow2,if=virtio" \
        -drive "file=$WORK/seed.img,format=raw,if=virtio" \
        -netdev "user,id=n0,hostfwd=tcp::$SSH_PORT-:22" \
        -device virtio-net-pci,netdev=n0 \
        -nographic -serial "file:$WORK/serial.log" -monitor none \
        > "$WORK/qemu.log" 2>&1 &
    QEMU_PID=$!
}

ssh_vm() {
    ssh -i "$WORK/id_vm" -p "$SSH_PORT" \
        -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
        -o LogLevel=ERROR -o ConnectTimeout=10 \
        "$VM_USER@127.0.0.1" "$@"
}

scp_vm() {
    scp -q -i "$WORK/id_vm" -P "$SSH_PORT" \
        -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
        -o LogLevel=ERROR "$1" "$VM_USER@127.0.0.1:$2"
}

# Poll until sshd answers, but give up rather than hang a CI job forever. A
# dead QEMU is detected directly, so a crash fails in seconds instead of
# waiting out the whole timeout.
wait_for_ssh() {
    local deadline=$((SECONDS + BOOT_TIMEOUT))
    while [ $SECONDS -lt $deadline ]; do
        kill -0 "$QEMU_PID" 2>/dev/null || fail "QEMU exited during boot — see $WORK/qemu.log"
        if ssh_vm true 2>/dev/null; then
            info "ssh is up after ${SECONDS}s"
            return 0
        fi
        sleep 3
    done
    fail "no ssh after ${BOOT_TIMEOUT}s — see $WORK/serial.log"
}

# Reboot and wait for it to come back: the only way to prove "starts at boot".
reboot_vm() {
    ssh_vm 'sudo systemctl reboot' 2>/dev/null || true
    sleep 10
    wait_for_ssh
}

shutdown_vm() {
    [ -n "${QEMU_PID:-}" ] || return 0
    ssh_vm 'sudo poweroff' 2>/dev/null || true
    for _ in $(seq 20); do
        kill -0 "$QEMU_PID" 2>/dev/null || return 0
        sleep 1
    done
    kill "$QEMU_PID" 2>/dev/null || true
}
