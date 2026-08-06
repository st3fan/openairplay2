# Install the package the way a user would, and check what only a real install
# can tell us. Sourced by ../vmtest with a booted VM and $DEB in scope.

assert() {
    local what=$1 expected=$2 actual=$3
    [ "$expected" = "$actual" ] || fail "$what: expected '$expected', got '$actual'"
    pass "$what = $actual"
}

assert_contains() {
    local what=$1 needle=$2 haystack=$3
    case $haystack in
        *"$needle"*) pass "$what contains '$needle'" ;;
        *) fail "$what: '$needle' not found in: $haystack" ;;
    esac
}

scp_vm "$DEB" /tmp/
deb_name=$(basename "$DEB")

# apt, not dpkg: this is the command the README gives users, and it is the one
# that resolves the dependency list the package declares.
info "apt-get install ./$deb_name"
ssh_vm "sudo apt-get update -qq && sudo apt-get install -y /tmp/$deb_name" \
    > "$WORK/install.log" 2>&1 \
    || { tail -20 "$WORK/install.log" >&2; fail "installation failed — full log: $WORK/install.log"; }
pass "package installed"

assert "unit is enabled" enabled "$(ssh_vm 'systemctl is-enabled openairplay2-receiver' 2>&1)"
assert "unit is active"  active  "$(ssh_vm 'systemctl is-active openairplay2-receiver' 2>&1)"

# The system user and its audio-group membership: without the group the daemon
# cannot open /dev/snd on a real machine.
user_line=$(ssh_vm 'id openairplay2' 2>&1) || fail "no openairplay2 user was created"
assert_contains "openairplay2 user" "(audio)" "$user_line"

# StateDirectory=, and proof the daemon actually ran: it writes its identity
# there on first start, and senders remember the receiver by that key.
assert "state dir owner" "openairplay2 openairplay2" \
    "$(ssh_vm 'stat -c "%U %G" /var/lib/openairplay2' 2>&1)"
ssh_vm 'test -s /var/lib/openairplay2/identity' \
    || fail "no identity file — the daemon never got far enough to write one"
pass "identity file written"
identity_before=$(ssh_vm 'sudo sha256sum /var/lib/openairplay2/identity | cut -d" " -f1')

# The options file must be a conffile, or a local edit is lost on upgrade.
conffiles=$(ssh_vm 'dpkg-query -W -f="\${Conffiles}" openairplay2-receiver' 2>&1)
assert_contains "conffiles" "/etc/default/openairplay2-receiver" "$conffiles"

# The daemon answers. GET /info is what a sender asks first, so a plist here
# means the whole startup path worked, not just that the process is alive.
info_code=$(ssh_vm 'curl -s -o /tmp/info.plist -w "%{http_code}" http://127.0.0.1:7000/info' 2>&1)
assert "GET /info status" 200 "$info_code"
assert_contains "GET /info body" "bplist00" "$(ssh_vm 'head -c 8 /tmp/info.plist' 2>&1)"

# Startup must be clean. The daemon logs a warning if avahi is not up; that is
# expected during the install transaction (avahi is a Recommends and may be
# configured after us), which is exactly why the reboot below re-checks it.
journal=$(ssh_vm 'sudo journalctl -u openairplay2-receiver --no-pager' 2>&1)
assert_contains "journal" "starting AirPlay 2 receiver" "$journal"
case $journal in
    *panicked*|*"Segmentation fault"*) fail "the daemon crashed — see the journal" ;;
esac
pass "no crash in the journal"

# "Starts at boot" is a claim the README makes; only a reboot proves it.
info "rebooting to check it comes back"
reboot_vm
assert "unit is active after reboot" active \
    "$(ssh_vm 'systemctl is-active openairplay2-receiver' 2>&1)"
assert "identity survived the reboot" "$identity_before" \
    "$(ssh_vm 'sudo sha256sum /var/lib/openairplay2/identity | cut -d" " -f1')"

# With avahi up from the start, the advertisement must succeed — this is the
# discovery path, so a silent failure here means no sender ever sees us.
boot_journal=$(ssh_vm 'sudo journalctl -u openairplay2-receiver --no-pager -b' 2>&1)
case $boot_journal in
    *"avahi advertisement disabled"*)
        fail "avahi advertisement failed on a clean boot:
$(echo "$boot_journal" | grep -i avahi | tail -3)" ;;
    *) pass "avahi advertisement did not fail on a clean boot" ;;
esac

# The upgrade promise from runbooks/releasing.md: removing the package keeps
# the identity, so a sender that paired before does not have to pair again.
ssh_vm 'sudo apt-get remove -y -qq openairplay2-receiver' >/dev/null 2>&1 \
    || fail "removal failed"
ssh_vm 'test -s /var/lib/openairplay2/identity' \
    || fail "removal deleted the pairing identity — an upgrade would force a re-pair"
pass "identity survives package removal"
