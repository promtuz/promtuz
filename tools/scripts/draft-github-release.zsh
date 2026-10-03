#!/usr/bin/env zsh
# Mirror a published release into a draft GitHub release on its tag.
# See --help or tools/scripts/README.md.

set -euo pipefail

SCRIPT="${0:A}"
REPO="$(git -C "${SCRIPT:h}" rev-parse --show-toplevel)"

BASE_URL="${PZ_UPDATE_URL:-https://apt.promtuz.dev}"
APT_CHANNEL="${PZ_APT_CHANNEL:-edge}"
ABIS=(arm64-v8a x86_64)
DEB_DIR="$REPO/target/x86_64-unknown-linux-gnu/debian"

TAG=""
EXTRA=""

_info() { print -r -- "  $*" }
_ok()   { print -r -- "✓ $*" }
_die()  { print -r -- "✗ $*" >&2; exit 1 }
_step() { print -r -- ""; print -r -- "── $* ──" }
_need() { command -v "$1" >/dev/null 2>&1 || _die "missing '$1'" }

_usage() {
    cat <<'EOF'
Usage: draft-github-release.zsh [TAG] [--extra FILE]

Creates a draft GitHub release on TAG (default: the newest v* tag) from what is
already published: the release-channel APKs on the update host and the daemon
.debs in target/ that the apt repo serves. Every file must match its published
copy. SHA256SUMS is signed with your git signing key.

  --extra FILE   Markdown placed after the app's notes, such as operator notes
  -h, --help     Show this help

PZ_UPDATE_URL and PZ_APT_CHANNEL (default edge) override defaults.
EOF
}

while (( $# )); do
    case "$1" in
        --extra)   [[ -r "${2:-}" ]] || _die "--extra needs a readable file"; EXTRA="${2:A}"; shift 2 ;;
        -h|--help) _usage; exit 0 ;;
        -*)        _die "unknown option $1 (see --help)" ;;
        *)         TAG="$1"; shift ;;
    esac
done

cd "$REPO"
_need gh; _need curl; _need openssl; _need gpg; _need shasum; _need xxd

[[ -n "$TAG" ]] || TAG="$(git tag --list 'v*' --sort=-v:refname | head -1)"
[[ -n "$TAG" ]] || _die "no v* tag to release"
VERSION="${TAG#v}"

_step "Preflight"
gh auth status >/dev/null 2>&1 || _die "gh is not signed in (gh auth login)"
git ls-remote --exit-code --tags origin "refs/tags/$TAG" >/dev/null || _die "$TAG is not on origin"
! gh release view "$TAG" >/dev/null 2>&1 || _die "a release for $TAG already exists"
_ok "$TAG is on origin and has no release yet"

SCRATCH="$(mktemp -d)"; chmod 700 "$SCRATCH"
trap 'rm -rf "$SCRATCH"' EXIT
OUT="$SCRATCH/assets"; mkdir "$OUT"

# Clients trust the key pinned in core, so the manifests are checked against it, not the vault.
key="$(sed -n '/const UPDATE_MANIFEST_PUBLIC_KEY/,/\];/p' libcore/src/api/update.rs \
    | grep -o '0x[0-9a-fA-F][0-9a-fA-F]' | sed 's/0x//' | tr -d '\n')"
(( ${#key} == 64 )) || _die "cannot read UPDATE_MANIFEST_PUBLIC_KEY from libcore/src/api/update.rs"
print -rn -- "302a300506032b6570032100$key" | xxd -r -p > "$SCRATCH/pin.der"

_verify() {
    openssl pkeyutl -verify -pubin -keyform DER -inkey "$SCRATCH/pin.der" -rawin \
        -in "$1" -sigfile "$2" >/dev/null 2>&1
}

# The release script writes one field per line.
_field() { sed -n "s/.*\"$1\": *\"*\([^\",]*\).*/\1/p" "$2" }

# ── Android ──────────────────────────────────────────────────────────────
_step "Android ($BASE_URL)"
typeset -A codes shas
for abi in $ABIS; do
    url="$BASE_URL/apk/release/$abi"
    curl -fsS -m 30 "$url/manifest.json"     -o "$SCRATCH/$abi.json" || _die "$abi manifest unreachable"
    curl -fsS -m 30 "$url/manifest.json.sig" -o "$SCRATCH/$abi.sig"  || _die "$abi manifest signature unreachable"
    _verify "$SCRATCH/$abi.json" "$SCRATCH/$abi.sig" || _die "$abi manifest does not verify against the app's key"
    name="$(_field versionName "$SCRATCH/$abi.json")"
    if [[ "$name" == "$VERSION" ]]; then
        codes[$abi]="$(_field versionCode "$SCRATCH/$abi.json")"
        shas[$abi]="$(_field sha256 "$SCRATCH/$abi.json")"
    else
        _info "$abi: the release channel serves $name"
    fi
done

APK_SIGNER=""
if (( ${#codes} == ${#ABIS} )); then
    export JAVA_HOME="${JAVA_HOME:-/Applications/Android Studio.app/Contents/jbr/Contents/Home}"
    SDK="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Library/Android/sdk}}"
    BUILD_TOOLS="$(/bin/ls -1 "$SDK/build-tools" 2>/dev/null | sort -V | tail -1)"
    APKSIGNER="$SDK/build-tools/$BUILD_TOOLS/apksigner"
    [[ -x "$APKSIGNER" ]] || _die "no apksigner in $SDK/build-tools/$BUILD_TOOLS"

    for abi in $ABIS; do
        apk="promtuz-$VERSION-$abi.apk"
        curl -fsS -m 600 "$BASE_URL/apk/release/$abi/promtuz-$VERSION~${codes[$abi]}.apk" -o "$OUT/$apk" \
            || _die "$abi APK download failed"
        [[ "$(shasum -a 256 "$OUT/$apk" | awk '{print $1}')" == "${shas[$abi]}" ]] \
            || _die "$apk does not match its signed manifest"
        signer="$("$APKSIGNER" verify --print-certs "$OUT/$apk" \
            | sed -n 's/.*certificate SHA-256 digest: //p' | head -1)" || _die "apksigner rejected $apk"
        [[ -n "$signer" && ( -z "$APK_SIGNER" || "$signer" == "$APK_SIGNER" ) ]] \
            || _die "$apk is not signed by the same certificate as the other ABIs"
        APK_SIGNER="$signer"
        _ok "$apk matches its signed manifest"
    done

    notes="$BASE_URL/apk/release/${ABIS[1]}/notes-${codes[${ABIS[1]}]}.md"
    http="$(curl -sS -m 30 -o "$SCRATCH/notes.md" -w '%{http_code}' "$notes")" || _die "app notes unreachable"
    case "$http" in
        200)
            curl -fsS -m 30 "$notes.sig" -o "$SCRATCH/notes.sig" || _die "app notes signature unreachable"
            _verify "$SCRATCH/notes.md" "$SCRATCH/notes.sig" || _die "app notes do not verify against the app's key"
            # Drop the "# VERSION - DATE" line the release script adds; the release title carries it.
            tail -n +3 "$SCRATCH/notes.md" > "$SCRATCH/app-notes.md"
            _ok "app notes verified" ;;
        404) _info "no app notes were published for this version" ;;
        *)   _die "app notes: HTTP $http" ;;
    esac
elif (( ${#codes} )); then
    _die "the ABIs disagree on the live version"
else
    _info "no APKs attached"
fi

# ── Server packages ──────────────────────────────────────────────────────
_step "Server packages (apt $APT_CHANNEL)"
debs=( "$DEB_DIR"/pz*_"$VERSION"-1_amd64.deb(N) )
if (( ${#debs} )); then
    curl -fsS -m 30 "$BASE_URL/dists/$APT_CHANNEL/main/binary-amd64/Packages" -o "$SCRATCH/Packages" \
        || _die "apt $APT_CHANNEL index unreachable"
    for deb in $debs; do
        pkg="${${deb:t}%%_*}"
        want="$(awk -v p="$pkg" -v v="$VERSION-1" \
            '/^Package:/ {n = $2} /^Version:/ {ver = $2} /^SHA256:/ && n == p && ver == v {print $2}' \
            "$SCRATCH/Packages")"
        [[ -n "$want" ]] || _die "$pkg $VERSION-1 is not published to apt $APT_CHANNEL"
        [[ "$(shasum -a 256 "$deb" | awk '{print $1}')" == "$want" ]] \
            || _die "${deb:t} differs from the copy apt $APT_CHANNEL serves"
        cp "$deb" "$OUT/"
        _ok "${deb:t} matches apt $APT_CHANNEL"
    done
else
    _info "no $VERSION packages in ${DEB_DIR#$REPO/}"
fi

(( ${#codes} || ${#debs} )) || _die "nothing for $VERSION is published"

# ── Checksums and notes ──────────────────────────────────────────────────
_step "Checksums"
(cd "$OUT" && shasum -a 256 -- *(.) > "$SCRATCH/SHA256SUMS") && mv "$SCRATCH/SHA256SUMS" "$OUT/"
signing_key="$(git config user.signingkey || true)"
gpg_user=()
if [[ -n "$signing_key" ]]; then gpg_user=(-u "$signing_key"); fi
gpg $gpg_user --armor --detach-sign "$OUT/SHA256SUMS" || _die "could not sign SHA256SUMS"
fpr="$(gpg --status-fd 1 --verify "$OUT/SHA256SUMS.asc" "$OUT/SHA256SUMS" 2>/dev/null \
    | awk '$2 == "VALIDSIG" {print $NF}')"
[[ -n "$fpr" ]] || _die "SHA256SUMS.asc does not verify"
_ok "SHA256SUMS signed by $fpr"

{
    if [[ -s "$SCRATCH/app-notes.md" ]]; then cat "$SCRATCH/app-notes.md"; print; fi
    if [[ -n "$EXTRA" ]]; then cat "$EXTRA"; print; fi
    print -r -- "### Downloads"
    if [[ -n "$APK_SIGNER" ]]; then
        print -r -- "- \`promtuz-$VERSION-arm64-v8a.apk\` is for phones. \`promtuz-$VERSION-x86_64.apk\` is for emulators and x86 Chromebooks. Once installed, Promtuz updates itself."
    fi
    if (( ${#debs} )); then
        print -r -- "- The \`.deb\` packages are for amd64 Debian 10+ and Ubuntu 18.04+, the same files ${BASE_URL#https://} serves."
    fi
    print -r -- "- \`SHA256SUMS\` lists every file's checksum and is signed with GPG key \`$fpr\`."
    if [[ -n "$APK_SIGNER" ]]; then
        print -r -- "- APK signing certificate SHA-256: \`$APK_SIGNER\`. Android refuses updates signed with any other certificate."
    fi
} > "$SCRATCH/release-notes.md"

# ── Draft ────────────────────────────────────────────────────────────────
_step "Draft"
gh release create "$TAG" --verify-tag --draft --title "Promtuz $VERSION" \
    --notes-file "$SCRATCH/release-notes.md" "$OUT"/*
_ok "draft created; publish it with: gh release edit $TAG --draft=false"
