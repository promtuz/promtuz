#!/bin/sh
# Promtuz installer — adds the apt repo (key + source) and installs one of the
# daemons. Debian / Ubuntu, amd64.
#
#   curl -fsSL https://apt.promtuz.dev/install.sh | sudo sh              # relay
#   curl -fsSL https://apt.promtuz.dev/install.sh | sudo sh -s resolver
#   curl -fsSL https://apt.promtuz.dev/install.sh | sudo sh -s gateway
#   curl -fsSL https://apt.promtuz.dev/install.sh | sudo CHANNEL=edge sh
#
# ── Why the whole thing is a function ────────────────────────────────────
# This is fetched and piped straight into sh, so a connection that drops
# mid-transfer hands sh a TRUNCATED script — which it will happily execute as
# far as it got. Half of this script leaves a keyring with no source list, or
# a source list pointing at a repo whose key never arrived. Defining main()
# and calling it on the very last line means a truncated download defines a
# function and does nothing at all.

set -eu

REPO_URL="https://apt.promtuz.dev"
CHANNEL="${CHANNEL:-edge}"
KEYRING="/etc/apt/keyrings/promtuz.asc"
LIST="/etc/apt/sources.list.d/promtuz.list"

# The repo signing key. Pinned so a wrong or stale keyring is caught rather
# than trusted silently. Signing keys get rotated; without a pin, a deliberate
# rotation is indistinguishable from a substitution.
#
# The pin has a known limit: an attacker able to tamper with the fetched
# keyring can usually tamper with this script too. What it does buy is an
# auditable constant, since this file is public, and detection of a
# half-finished deploy serving a key that does not match the published repo.
EXPECTED_FPR="5C26AD22B4BA8DA4EA0A8FDE16A1AAFCDEC1E184"

say()  { printf '%s\n' "$*"; }
warn() { printf '! %s\n' "$*" >&2; }
die()  { printf 'error: %s\n' "$*" >&2; exit 1; }

need_root() {
    [ "$(id -u)" -eq 0 ] || die "run as root (e.g. pipe to 'sudo sh')."
}

check_platform() {
    command -v apt-get >/dev/null 2>&1 || die "this repo is Debian/Ubuntu (apt) only."

    # The repo publishes amd64 only. Without this, an arm64 box gets
    # "Unable to locate package pzrelay", which reads like a broken repo
    # rather than an unpublished architecture.
    arch="$(dpkg --print-architecture 2>/dev/null || echo unknown)"
    if [ "$arch" != "amd64" ]; then
        die "this machine is '$arch'; the repo currently publishes amd64 only.
       Build from source, or open an issue asking for $arch packages."
    fi
}

ensure_tools() {
    # gnupg is needed to read the key's fingerprint. Minimal server images
    # ship gpgv (apt needs it) but frequently not gpg.
    missing=""
    command -v curl >/dev/null 2>&1 || missing="$missing curl"
    command -v gpg  >/dev/null 2>&1 || missing="$missing gnupg"
    if [ -n "$missing" ]; then
        say "Installing:$missing"
        apt-get update -qq
        # shellcheck disable=SC2086
        apt-get install -y -qq ca-certificates $missing
    fi
}

fetch_key() {
    tmp_key="$(mktemp)"
    curl -fsSL "$REPO_URL/promtuz-archive-keyring.asc" -o "$tmp_key" \
        || die "could not download the signing key from $REPO_URL"

    got="$(gpg --show-keys --with-colons "$tmp_key" 2>/dev/null \
           | awk -F: '/^fpr:/ {print $10; exit}')"
    [ -n "$got" ] || die "the downloaded keyring is not a valid PGP key."

    if [ "$got" != "$EXPECTED_FPR" ]; then
        rm -f "$tmp_key"
        die "signing key fingerprint mismatch.
       expected $EXPECTED_FPR
       got      $got
       Refusing to trust it. If the key was rotated deliberately, get the
       current installer; otherwise treat this as a compromise."
    fi

    # Replacing an existing pin is normal after a rotation, but it is reported:
    # a change to the key a host trusts must never pass unannounced.
    if [ -f "$KEYRING" ]; then
        old="$(gpg --show-keys --with-colons "$KEYRING" 2>/dev/null \
               | awk -F: '/^fpr:/ {print $10; exit}')"
        if [ -n "$old" ] && [ "$old" != "$got" ]; then
            warn "replacing the previously trusted key:"
            warn "  was $old"
            warn "  now $got"
        fi
    fi

    install -d -m 0755 /etc/apt/keyrings
    install -m 0644 "$tmp_key" "$KEYRING"
    rm -f "$tmp_key"
    say "Signing key: $got"
}

add_source() {
    # arch=amd64 is explicit: on a multi-arch box apt would otherwise look for
    # an arm64 index that does not exist and warn on every update.
    printf 'deb [arch=amd64 signed-by=%s] %s %s main\n' "$KEYRING" "$REPO_URL" "$CHANNEL" > "$LIST"
    say "Source: $REPO_URL $CHANNEL main"
}

install_pkg() {
    pkg="$1"
    apt-get update
    # unattended-upgrades named explicitly: the deb only Recommends it, and
    # minimal server images skip Recommends. The package ships an apt.conf.d
    # fragment that lets it auto-upgrade promtuz packages daily.
    apt-get install -y "$pkg" unattended-upgrades
}

verify_install() {
    pkg="$1"
    dpkg-query -W -f='${Status}' "$pkg" 2>/dev/null | grep -q "ok installed" \
        || die "$pkg did not install cleanly."
    ver="$(dpkg-query -W -f='${Version}' "$pkg" 2>/dev/null)"
    say "Installed: $pkg $ver"

    # Only the relay package ships /etc/promtuz/ca.pem — dpkg will not let two
    # packages own one path — so installing resolver or gateway on a clean box
    # leaves them with no CA to verify against, and they cannot start. Catch it
    # here rather than letting the daemon fail with something less obvious.
    if [ ! -f /etc/promtuz/ca.pem ]; then
        warn "/etc/promtuz/ca.pem is missing — $pkg cannot verify peers without it."
        warn "Copy the root CA there from your CA box, then: systemctl restart $pkg"
    fi
}

next_steps() {
    role="$1" pkg="$2"
    say ""
    case "$role" in
        relay|gateway|resolver)
            cat <<EOF
$pkg is installed and started, waiting for its certificate.
One step left, on your CA box:

  1. keys ca-sign /etc/promtuz/keys/$role/$role.csr --cap $role > $role.crt
  2. drop it at /etc/promtuz/certs/$role.crt      # it starts serving automatically

Edit /etc/promtuz/$role.toml for a non-default resolver seed or bind address.
Updates apply automatically (unattended-upgrades, daily); config is preserved.
EOF
            ;;
    esac
}

main() {
    role="${1:-relay}"
    case "$role" in
        relay|resolver|gateway) pkg="pz$role" ;;
        *) die "unknown role '$role' — expected relay, resolver or gateway." ;;
    esac

    need_root
    check_platform
    ensure_tools
    fetch_key
    add_source
    install_pkg "$pkg"
    verify_install "$pkg"
    next_steps "$role" "$pkg"
}

# Last line on purpose — see the note at the top about truncated downloads.
main "$@"
