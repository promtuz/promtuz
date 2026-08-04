#!/usr/bin/env zsh
# Build the relay/resolver/gateway .debs, rebuild the apt repo, sign it with
# the vaulted key, and publish.
#
#   publish-apt.zsh                      bump prompt, build, publish to 'edge'
#   publish-apt.zsh --version 0.3.1      skip the prompt
#   publish-apt.zsh --no-publish         build + sign locally, upload nothing
#   publish-apt.zsh --dry-run            say what would happen, touch nothing
#
# ── Why the whole repo gets rebuilt ──────────────────────────────────────
# No aptly database is carried between runs, so the index is regenerated from
# the packages built here rather than appended to. Pruning follows from that,
# and is wanted: relay/Cargo.toml ships ../.tls/RootCA.pem as
# /etc/promtuz/ca.pem, so any package built under a superseded CA installs a
# node that can never enrol. Such packages must not stay reachable.
#
# ── What it must not break ───────────────────────────────────────────────
# /var/www/apt also holds apk/, install.sh and the keyring. Only dists/ and
# pool/ are synced, and --delete is scoped to exactly those two — a --delete
# against the docroot would take the Android releases with it.
#
# The published Release is signed by the key in the vault. When that key
# differs from the one a host already pinned via signed-by=, `apt update` there
# fails until /etc/apt/keyrings/promtuz.asc is replaced; the matching public
# half is published alongside the repo so that remains a one-liner.

set -euo pipefail

SCRIPT="${0:A}"
REPO="$(git -C "${SCRIPT:h}" rev-parse --show-toplevel)"

VAULT="${PZ_VAULT:-$HOME/.promtuz-vault}"
BASE_URL="${PZ_APT_URL:-https://apt.promtuz.dev}"
PUBLISH_HOST="${PZ_PUBLISH_HOST:-promtuz@apt.promtuz.dev}"
PUBLISH_ROOT="${PZ_PUBLISH_ROOT:-/var/www/apt}"

CHANNEL="${PZ_APT_CHANNEL:-edge}"
CRATES=(relay resolver gateway)
TARGET=x86_64-unknown-linux-gnu
GLIBC=2.28

VERSION=""
PUBLISH=1
DRY_RUN=0

_info() { print -r -- "  $*" }
_ok()   { print -r -- "✓ $*" }
_warn() { print -r -- "! $*" >&2 }
_die()  { print -r -- "✗ $*" >&2; exit 1 }
_step() { print -r -- ""; print -r -- "── $* ──" }
_need() { command -v "$1" >/dev/null 2>&1 || _die "missing '$1'" }

_confirm() {
    [[ -t 0 ]] || _die "$1 (not a terminal — pass the flag explicitly)"
    local reply
    printf '%s [y/N] ' "$1" >&2
    read -r reply
    [[ "$reply" == [yY]* ]] || _die "aborted"
}

# Retry a mistyped passphrase instead of losing the whole run. age exits 130
# when its prompt is interrupted, so Ctrl-C still aborts immediately.
_unlock_vault() {
    local out="$1" src="$2" tries=3 attempt rc
    for attempt in {1..$tries}; do
        age -d -o "$out" "$src"
        rc=$?
        (( rc == 0 )) && return 0
        (( rc == 130 )) && _die "cancelled"
        (( attempt < tries )) && _warn "wrong passphrase — $(( tries - attempt )) attempt(s) left"
    done
    _die "could not unlock the vault after $tries attempts"
}

while (( $# )); do
    case "$1" in
        --version)    VERSION="${2:?}"; shift 2 ;;
        --channel)    CHANNEL="${2:?}"; shift 2 ;;
        --no-publish) PUBLISH=0; shift ;;
        --dry-run)    DRY_RUN=1; PUBLISH=0; shift ;;
        -h|--help)    sed -n '2,9p' "$SCRIPT" | sed 's/^# \{0,1\}//'; exit 0 ;;
        *)            _die "unknown option '$1'" ;;
    esac
done

# ── preflight ────────────────────────────────────────────────────────────
_step "Preflight"

_need age; _need aptly; _need gpg; _need zig; _need rsync; _need curl; _need ar; _need tar
command -v cargo-zigbuild >/dev/null || _die "missing cargo-zigbuild"
cargo deb --version >/dev/null 2>&1 || _die "missing cargo-deb (cargo install cargo-deb)"
rustup target list --installed | grep -qx "$TARGET" || _die "rust target $TARGET not installed"

[[ -e "$VAULT/apt-signing-secret.asc.age" ]] || _die "no APT key in the vault — run 'keys mint-apt'"
[[ -e "$VAULT/promtuz-archive-keyring.asc" ]] || _die "vault has no promtuz-archive-keyring.asc"
[[ -e "$VAULT/identity.age" ]] || _die "no vault at $VAULT"

# The whole point of this republish: the .debs must carry the CURRENT CA.
[[ -e "$REPO/.tls/RootCA.pem" ]] || _die "no .tls/RootCA.pem — run 'keys install-ca'"
if [[ -e "$VAULT/RootCA.pem" ]] && ! cmp -s "$VAULT/RootCA.pem" "$REPO/.tls/RootCA.pem"; then
    _die ".tls/RootCA.pem differs from the vault's — run 'keys install-ca' before publishing"
fi
CA_FPR="$(openssl x509 -in "$REPO/.tls/RootCA.pem" -noout -fingerprint -sha256 | cut -d= -f2)"
_info "root CA   $CA_FPR"

# install.sh pins the repo signing key's fingerprint. If that constant and the
# key being published disagree, every fresh install aborts on a mismatch — so
# the two are checked against each other here rather than in the field.
INSTALLER="$REPO/tools/pkg/install.sh"
[[ -f "$INSTALLER" ]] || _die "no installer at $INSTALLER"
PINNED="$(sed -n 's/^EXPECTED_FPR="\(.*\)"/\1/p' "$INSTALLER")"
VAULT_FPR="$(gpg --show-keys --with-colons "$VAULT/promtuz-archive-keyring.asc" 2>/dev/null \
    | awk -F: '/^fpr:/ {print $10; exit}')"
[[ -n "$VAULT_FPR" ]] || _die "the vault's promtuz-archive-keyring.asc is not a valid PGP key"
[[ "$PINNED" == "$VAULT_FPR" ]] || _die "install.sh pins a different signing key than the vault holds:
    install.sh  $PINNED
    vault       $VAULT_FPR
    Update EXPECTED_FPR in tools/pkg/install.sh, or publish the pinned key."
_info "repo key  $VAULT_FPR (matches the installer's pin)"

if [[ -n "$(git -C "$REPO" status --porcelain)" ]]; then
    _warn "working tree is dirty — these packages will not be reproducible from git"
fi
_info "commit    $(git -C "$REPO" rev-parse --short HEAD)"
_info "channel   $CHANNEL"
_ok "preflight passed"

# ── version ──────────────────────────────────────────────────────────────
_step "Version"

CURRENT="$(sed -n 's/^version = "\(.*\)"/\1/p' "$REPO/Cargo.toml" | head -1)"
_info "workspace version: $CURRENT"

# What apt already serves. Publishing a version that is already out there is
# the quiet failure mode: same version string, different bytes, and every
# existing box reports "already up to date" while still trusting a dead CA.
live_versions=()
pkgs="$(curl -sS -m 20 "$BASE_URL/dists/$CHANNEL/main/binary-amd64/Packages" 2>/dev/null || true)"
if [[ -n "$pkgs" ]]; then
    live_versions=(${(f)"$(print -r -- "$pkgs" | sed -n 's/^Version: //p' | sort -u)"})
    _info "published: ${live_versions[*]}"
fi

if [[ -z "$VERSION" ]]; then
    VERSION="$CURRENT"
    if [[ -t 0 ]]; then
        vared -p "  version: " VERSION
    fi
fi
VERSION="${VERSION## }"; VERSION="${VERSION%% }"
[[ -n "$VERSION" ]] || _die "version is empty"

# cargo-deb maps a semver prerelease to a debian tilde: 0.3.1-rc1 -> 0.3.1~rc1-1
DEB_VERSION="${VERSION//-/\~}-1"
if (( ${live_versions[(I)$DEB_VERSION]} )); then
    _die "$DEB_VERSION is already published. apt upgrades on the version string, so
    republishing it would leave every existing box on the old package —
    still trusting the dead CA. Pick a higher version."
fi
_info "building  $VERSION  (deb: $DEB_VERSION)"

if (( DRY_RUN )); then
    print -r -- ""
    _ok "dry run — would build ${CRATES[*]} at $DEB_VERSION and publish to $CHANNEL"
    exit 0
fi

# ── build ────────────────────────────────────────────────────────────────
_step "Build"

if [[ "$VERSION" != "$CURRENT" ]]; then
    # Only the workspace's own version line; the daemons all inherit it.
    sed -i.bak "0,/^version = \"$CURRENT\"/s//version = \"$VERSION\"/" "$REPO/Cargo.toml"
    rm -f "$REPO/Cargo.toml.bak"
    _info "Cargo.toml -> $VERSION"
fi

SCRATCH="$(mktemp -d)"; chmod 700 "$SCRATCH"
trap 'rm -rf "$SCRATCH"' EXIT INT TERM
DEBS="$SCRATCH/debs"; mkdir -p "$DEBS"

for crate in $CRATES; do
    _info "building $crate…"
    (
        cd "$REPO"
        # cargo-zigbuild links an old glibc + static libstdc++/libgcc, which is
        # what makes `depends = libc6 (>= 2.28)` an honest claim. A plain
        # `cargo deb` would rebuild against the host and mislabel it.
        cargo zigbuild --release -p "$crate" --target "${TARGET}.${GLIBC}" >/dev/null
        cargo deb -p "$crate" --no-build --target "$TARGET" >/dev/null
    ) || _die "$crate failed to build"
done

# cargo-deb never cleans its output directory, so it still holds every package
# ever built here. Take only this version's, and insist on one per crate — a
# bare *.deb would sweep up a previous release, and would also let a crate whose
# `cargo deb` silently produced nothing pass as built.
for f in "$REPO/target/$TARGET/debian/"*_"${DEB_VERSION}"_*.deb(N); do
    cp "$f" "$DEBS/"
done
built=("$DEBS"/*.deb(N))
(( $#built == $#CRATES )) \
    || _die "expected $#CRATES packages at $DEB_VERSION, found $#built"
_ok "built $#built packages"

# ── verify the packages ──────────────────────────────────────────────────
_step "Verify packages"

# Pull a file out of a .deb without dpkg — macOS has ar and tar, not dpkg-deb.
_deb_extract() {
    local deb="$1" want="$2" out="$3" d="$SCRATCH/x"
    rm -rf "$d"; mkdir -p "$d"
    (cd "$d" && ar x "$deb")
    local data
    data="$(/bin/ls "$d"/data.tar.* 2>/dev/null | head -1)"
    [[ -n "$data" ]] || return 1
    tar -xOf "$data" "./$want" > "$out" 2>/dev/null || return 1
    [[ -s "$out" ]]
}

for deb in "$DEBS"/*.deb; do
    name="${deb:t}"
    [[ "$name" == *"${DEB_VERSION%-1}"* ]] \
        || _die "$name does not carry version ${DEB_VERSION%-1}"

    # The relay ships the CA. This is the check the whole republish exists for:
    # a package carrying the dead CA is worse than no package, because the node
    # installs, starts, and waits forever for an enrolment that cannot come.
    if [[ "$name" == pzrelay* ]]; then
        _deb_extract "$deb" "etc/promtuz/ca.pem" "$SCRATCH/deb-ca.pem" \
            || _die "$name has no /etc/promtuz/ca.pem"
        cmp -s "$SCRATCH/deb-ca.pem" "$REPO/.tls/RootCA.pem" \
            || _die "$name ships a DIFFERENT root CA than .tls/RootCA.pem"
        _ok "$name  carries the current CA"
    else
        _ok "$name"
    fi
done

# ── sign + build the index ───────────────────────────────────────────────
_step "Repository"

_info "unlocking the vault…"
_unlock_vault "$SCRATCH/identity" "$VAULT/identity.age"
chmod 600 "$SCRATCH/identity"
age -d -i "$SCRATCH/identity" -o "$SCRATCH/apt-secret.asc" "$VAULT/apt-signing-secret.asc.age"

# Throwaway keyring: the repo key never enters the invoking user's own, and
# nothing lingers once the trap fires.
export GNUPGHOME="$SCRATCH/gnupg"
mkdir -p "$GNUPGHOME"; chmod 700 "$GNUPGHOME"
gpg --batch --quiet --import "$SCRATCH/apt-secret.asc" 2>/dev/null \
    || _die "could not import the APT signing key"
APT_FPR="$(gpg --batch --list-secret-keys --with-colons | awk -F: '/^fpr:/ {print $10; exit}')"
[[ -n "$APT_FPR" ]] || _die "no secret key after import"
_ok "signing key $APT_FPR"

APTLY_ROOT="$SCRATCH/aptly"
cat > "$SCRATCH/aptly.conf" <<EOF
{
  "rootDir": "$APTLY_ROOT",
  "architectures": ["amd64"],
  "gpgProvider": "gpg"
}
EOF
A=(aptly -config="$SCRATCH/aptly.conf")

"${A[@]}" repo create -distribution="$CHANNEL" -component=main promtuz >/dev/null
"${A[@]}" repo add promtuz "$DEBS" >/dev/null 2>&1 || _die "aptly could not add the packages"
"${A[@]}" publish repo -batch -architectures=amd64 -distribution="$CHANNEL" \
    -gpg-key="$APT_FPR" -passphrase="" promtuz >/dev/null \
    || _die "aptly could not publish"

PUB="$APTLY_ROOT/public"
[[ -d "$PUB/dists/$CHANNEL" ]] || _die "aptly produced no dists/$CHANNEL"

# Verify the generated index before it reaches anyone. The keyring must be a real
# file — gpgv seeks it, so a process substitution fails in a way that looks
# exactly like a bad signature.
gpg --batch --export "$APT_FPR" > "$SCRATCH/apt-pub.gpg"
if [[ -f "$PUB/dists/$CHANNEL/Release.gpg" ]]; then
    gpgv --keyring "$SCRATCH/apt-pub.gpg" \
         "$PUB/dists/$CHANNEL/Release.gpg" "$PUB/dists/$CHANNEL/Release" 2>"$SCRATCH/gpgv.err" \
        || _die "the Release we just signed does not verify against our own key:
$(sed 's/^/    /' "$SCRATCH/gpgv.err")"
elif [[ -f "$PUB/dists/$CHANNEL/InRelease" ]]; then
    gpgv --keyring "$SCRATCH/apt-pub.gpg" "$PUB/dists/$CHANNEL/InRelease" 2>"$SCRATCH/gpgv.err" \
        || _die "the InRelease we just signed does not verify against our own key:
$(sed 's/^/    /' "$SCRATCH/gpgv.err")"
else
    _die "aptly produced neither Release.gpg nor InRelease — the repo would be unsigned"
fi
_ok "index built and signed"
_info "packages: $(sed -n 's/^Package: //p' "$PUB/dists/$CHANNEL/main/binary-amd64/Packages" | sort -u | tr '\n' ' ')"

if (( ! PUBLISH )); then
    KEEP="$REPO/target/apt-staging"
    rm -rf "$KEEP"; mkdir -p "$KEEP"
    cp -R "$PUB"/. "$KEEP"/
    cp "$VAULT/promtuz-archive-keyring.asc" "$KEEP/"
    print -r -- ""
    _ok "staged at $KEEP (nothing published)"
    exit 0
fi

# ── publish ──────────────────────────────────────────────────────────────
_step "Publish"

print -r -- ""
_warn "This replaces the repo signature. Boxes pinned to the OLD keyring will"
_warn "fail 'apt update' until /etc/apt/keyrings/promtuz.asc is replaced."
_warn "Old package versions will be DELETED from $PUBLISH_ROOT/pool."
_confirm "Publish to $PUBLISH_HOST:$PUBLISH_ROOT?"

ssh "$PUBLISH_HOST" "mkdir -p '$PUBLISH_ROOT/dists' '$PUBLISH_ROOT/pool'" \
    || _die "could not prepare the remote directories"

# --delete, but scoped to dists/ and pool/ ONLY. The docroot also holds apk/
# and install.sh; a --delete against it would erase the Android releases.
rsync -a --delete "$PUB/pool/"  "$PUBLISH_HOST:$PUBLISH_ROOT/pool/"  || _die "pool sync failed"
rsync -a --delete "$PUB/dists/" "$PUBLISH_HOST:$PUBLISH_ROOT/dists/" || _die "dists sync failed"
rsync -a "$VAULT/promtuz-archive-keyring.asc" "$PUBLISH_HOST:$PUBLISH_ROOT/" || _die "keyring upload failed"
# Ship the installer from the repo so the served copy cannot drift from the
# source, which is how it ends up pinning a key that is no longer published.
rsync -a "$INSTALLER" "$PUBLISH_HOST:$PUBLISH_ROOT/install.sh" || _die "installer upload failed"
_ok "uploaded"

# ── confirm live ─────────────────────────────────────────────────────────
_step "Confirm live"

curl -fsS -m 30 "$BASE_URL/promtuz-archive-keyring.asc" -o "$SCRATCH/live-key.asc" \
    || _die "the published keyring is not reachable"
cmp -s "$SCRATCH/live-key.asc" "$VAULT/promtuz-archive-keyring.asc" \
    || _die "the published keyring is not the vault's"

curl -fsS -m 30 "$BASE_URL/install.sh" -o "$SCRATCH/live-install.sh" \
    || _die "the published installer is not reachable"
cmp -s "$SCRATCH/live-install.sh" "$INSTALLER" \
    || _die "the published install.sh differs from tools/pkg/install.sh"

curl -fsS -m 30 "$BASE_URL/dists/$CHANNEL/Release"     -o "$SCRATCH/live-rel"     || _die "Release unreachable"
curl -fsS -m 30 "$BASE_URL/dists/$CHANNEL/Release.gpg" -o "$SCRATCH/live-rel.gpg" || _die "Release.gpg unreachable"

# Verify exactly as apt would: the live Release against the live keyring.
# --dearmor gives gpgv a plain binary keyring, which is what it wants; an
# imported keybox is not always readable by it.
gpg --dearmor < "$SCRATCH/live-key.asc" > "$SCRATCH/live-key.gpg" 2>/dev/null \
    || _die "the published keyring is not a valid PGP key"
gpgv --keyring "$SCRATCH/live-key.gpg" "$SCRATCH/live-rel.gpg" "$SCRATCH/live-rel" 2>"$SCRATCH/gpgv2.err" \
    || _die "the live Release does not verify against the live keyring — apt would refuse this repo:
$(sed 's/^/    /' "$SCRATCH/gpgv2.err")"
_ok "live Release verifies against the published keyring"

for crate in $CRATES; do
    pkg="pz$crate"
    curl -fsS -m 30 "$BASE_URL/dists/$CHANNEL/main/binary-amd64/Packages" 2>/dev/null \
        | grep -q "^Package: $pkg\$" || _die "$pkg missing from the live index"
done
_ok "all three packages in the live index"

print -r -- ""
_ok "published $DEB_VERSION to $CHANNEL"
print -r -- ""
print -r -- "On every existing box, replace the pinned keyring:"
print -r -- "  sudo curl -fsSL $BASE_URL/promtuz-archive-keyring.asc -o /etc/apt/keyrings/promtuz.asc"
print -r -- "  sudo apt update && sudo apt install --only-upgrade pzrelay pzresolver pzgateway"
print -r -- ""
_info "commit the version bump:"
_info "  git add Cargo.toml Cargo.lock && git commit -m 'chore: release $VERSION'"
