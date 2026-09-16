#!/usr/bin/env zsh
# Build, sign, publish, and announce an Android release.
# See --help or tools/scripts/README.md for setup and retry commands.

set -euo pipefail

SCRIPT="${0:A}"
REPO="$(git -C "${SCRIPT:h}" rev-parse --show-toplevel)"

VAULT="${PZ_VAULT:-$HOME/.promtuz-vault}"
BASE_URL="${PZ_UPDATE_URL:-https://apt.promtuz.dev}"
PUBLISH_HOST="${PZ_PUBLISH_HOST:-promtuz@apt.promtuz.dev}"
PUBLISH_ROOT="${PZ_PUBLISH_ROOT:-/var/www/apt}"

ABIS=(arm64-v8a x86_64)
ALL_CHANNELS=(release debug)

CHANNEL=debug
VERSION_NAME=""
VERSION_CODE=""
PUBLISH=1
DRY_RUN=0
NOTIFY=1
NOTIFY_ONLY=0

_info() { print -r -- "  $*" }
_ok()   { print -r -- "✓ $*" }
_warn() { print -r -- "! $*" >&2 }
_die()  { print -r -- "✗ $*" >&2; exit 1 }
_step() { print -r -- ""; print -r -- "── $* ──" }
_need() { command -v "$1" >/dev/null 2>&1 || _die "missing '$1'" }

_usage() {
    cat <<'EOF'
Usage: release-android.zsh [options]

  --channel debug|release|both  Target channel (default: debug)
  --version-name VERSION       Skip the version prompt
  --version-code CODE          Use an explicit code above published versions
  --no-publish                 Build, sign, and stage locally
  --no-notify                  Publish without a release announcement
  --notify-only                Announce the current live release; skip the build
  --dry-run                    Print the plan; no network, credentials, or writes
  -h, --help                   Show this help

Announcements use GOOGLE_APPLICATION_CREDENTIALS (FCM service-account JSON).
PZ_VAULT, PZ_UPDATE_URL, PZ_PUBLISH_HOST, and PZ_PUBLISH_ROOT override defaults.
EOF
}

_value() {
    [[ -n "$2" && "$2" != --* ]] || _die "$1 needs a value (see --help)"
}

# Interactive yes/no. Anything but an explicit yes aborts — these gates guard
# publishing to real users, so silence must never mean consent.
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
        if age -d -o "$out" "$src"; then
            return 0
        else
            rc=$?
        fi
        (( rc == 130 )) && _die "cancelled"
        rm -f "$out"
        (( attempt < tries )) && _warn "wrong passphrase — $(( tries - attempt )) attempt(s) left"
    done
    _die "could not unlock the vault after $tries attempts"
}

while (( $# )); do
    case "$1" in
        --channel)      _value "$1" "${2-}"; CHANNEL="$2"; shift 2 ;;
        --version-name) _value "$1" "${2-}"; VERSION_NAME="$2"; shift 2 ;;
        --version-code) _value "$1" "${2-}"; VERSION_CODE="$2"; shift 2 ;;
        --no-publish)   PUBLISH=0; shift ;;
        --no-notify)    NOTIFY=0; shift ;;
        --notify-only)  NOTIFY_ONLY=1; shift ;;
        --dry-run)      DRY_RUN=1; shift ;;
        -h|--help)      _usage; exit 0 ;;
        *)              _die "unknown option '$1'" ;;
    esac
done

case "$CHANNEL" in
    release) TARGETS=(release) ;;
    debug)   TARGETS=(debug) ;;
    both)    TARGETS=(release debug) ;;
    *)       _die "channel must be release, debug, or both" ;;
esac

if [[ -n "$VERSION_CODE" ]]; then
    [[ "$VERSION_CODE" =~ '^[1-9][0-9]{0,9}$' ]] && (( VERSION_CODE <= 2100000000 )) \
        || _die "versionCode must be an integer from 1 to 2100000000"
fi
if [[ -n "$VERSION_NAME" ]]; then
    [[ "$VERSION_NAME" =~ '^[A-Za-z0-9][A-Za-z0-9._+-]*$' ]] || _die "invalid versionName '$VERSION_NAME'"
fi

if (( NOTIFY_ONLY )); then
    (( NOTIFY && PUBLISH )) || _die "--notify-only cannot be combined with --no-notify or --no-publish"
    [[ -z "$VERSION_NAME" && -z "$VERSION_CODE" ]] || _die "--notify-only uses the current live version; omit version flags"
    _need python3
    notify_args=(--channel "$CHANNEL")
    (( DRY_RUN )) && notify_args+=(--dry-run)
    exec python3 "${SCRIPT:h}/notify-android.py" "${notify_args[@]}"
fi

if (( DRY_RUN )); then
    _info "channels: ${TARGETS[*]}"
    _info "version: ${VERSION_NAME:-prompt at release time}"
    _info "versionCode: ${VERSION_CODE:-next code above local and published versions}"
    _info "build and sign: ${ABIS[*]}"
    if (( ! PUBLISH )); then
        _info "stage locally: $REPO/android/app/build/release-staging (no upload or announcement)"
    elif (( NOTIFY )); then
        _info "publish destination: $PUBLISH_HOST:$PUBLISH_ROOT/apk"
        _info "announce after all live checks pass (GOOGLE_APPLICATION_CREDENTIALS)"
    else
        _info "publish destination: $PUBLISH_HOST:$PUBLISH_ROOT/apk"
        _info "release announcements disabled"
    fi
    exit 0
fi

# ── preflight ────────────────────────────────────────────────────────────
_step "Preflight"

_need age; _need curl; _need openssl; _need rsync; _need xxd
if (( PUBLISH && NOTIFY )); then
    _need python3
    python3 "${SCRIPT:h}/notify-android.py" --check-credentials \
        || _die "announcement setup is incomplete; fix it or use --no-notify"
fi

# Exported, not just assigned: apksigner is a shell wrapper that resolves java
# from the ENVIRONMENT. A plain assignment leaves it unable to find a runtime,
# and it then exits 1 with a message that never reaches the caller.
export JAVA_HOME="${JAVA_HOME:-/Applications/Android Studio.app/Contents/jbr/Contents/Home}"
[[ -x "$JAVA_HOME/bin/keytool" ]] || _die "no JDK at JAVA_HOME=$JAVA_HOME"
export PATH="$JAVA_HOME/bin:$PATH"

SDK="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/Library/Android/sdk}}"
[[ -d "$SDK" ]] || _die "no Android SDK at $SDK — set ANDROID_HOME"
BUILD_TOOLS="$(/bin/ls -1 "$SDK/build-tools" 2>/dev/null | sort -V | tail -1)"
APKSIGNER="$SDK/build-tools/$BUILD_TOOLS/apksigner"
[[ -x "$APKSIGNER" ]] || _die "no apksigner in $SDK/build-tools/$BUILD_TOOLS"

# cargo-ndk needs the Android std targets, and Gradle shells out to a bare
# `cargo` — so what matters is the ACTIVE toolchain, not whether the targets
# exist somewhere. Having them on stable while nightly is default fails deep
# inside Gradle with an error that never names the real cause.
ACTIVE_TC="$(rustup show active-toolchain 2>/dev/null | awk '{print $1}')"
for t in aarch64-linux-android x86_64-linux-android; do
    rustup target list --installed 2>/dev/null | grep -qx "$t" && continue

    owner=""
    for tc in $(rustup toolchain list 2>/dev/null | awk '{print $1}'); do
        if rustup target list --installed --toolchain "$tc" 2>/dev/null | grep -qx "$t"; then
            owner="$tc"
            break
        fi
    done

    if [[ -n "$owner" ]]; then
        print -r -- "✗ rust target $t is installed on '$owner', but the active toolchain is '$ACTIVE_TC'." >&2
        print -r -- "  Gradle shells out to a bare 'cargo', so it will use '$ACTIVE_TC' and fail." >&2
        print -r -- "  Either add the target to the active toolchain:" >&2
        print -r -- "      rustup target add $t" >&2
        print -r -- "  or pin this repo to the toolchain that already has it (also fixes the Arch box):" >&2
        print -r -- "      echo '[toolchain]\\nchannel = \"${owner%%-*}\"' > $REPO/rust-toolchain.toml" >&2
        exit 1
    fi
    _die "rust target $t not installed — rustup target add $t"
done

[[ -e "$VAULT/identity.age" ]] || _die "no vault at $VAULT — run 'keys clone <remote>'"
for item in android-release.p12.age android-keystore.properties.age \
            update-manifest.key.age update-manifest.pub; do
    [[ -e "$VAULT/$item" ]] || _die "vault is missing $item"
done

if [[ -n "$(git -C "$REPO" status --porcelain)" ]]; then
    _warn "working tree is dirty — this build will not be reproducible from git"
    git -C "$REPO" status --short | sed 's/^/    /' >&2
fi
_info "commit    $(git -C "$REPO" rev-parse --short HEAD)"
_info "channels  ${TARGETS[*]}"
_info "sdk       $SDK (build-tools $BUILD_TOOLS)"
_ok "preflight passed"

# ── version ──────────────────────────────────────────────────────────────
_step "Version"

GRADLE_PROPS="$REPO/android/gradle.properties"
CURRENT_NAME="$(sed -n 's/^promtuzVersionName=//p' "$GRADLE_PROPS")"
CURRENT_CODE="$(sed -n 's/^promtuzVersionCode=//p' "$GRADLE_PROPS")"

published_max=0
published_names=()
latest_name="$CURRENT_NAME"
for ch in $ALL_CHANNELS; do
    for abi in $ABIS; do
        live="$(curl -sS -m 15 "$BASE_URL/apk/$ch/$abi/manifest.json" 2>/dev/null | tr -d '\n ')"
        code="$(print -r -- "$live" | sed -n 's/.*"versionCode":\([0-9]*\).*/\1/p')"
        name="$(print -r -- "$live" | sed -n 's/.*"versionName":"\([^"]*\)".*/\1/p')"
        [[ -n "$code" ]] || continue
        _info "live  $ch/$abi  versionCode $code  ($name)"
        published_names+=("$name")
        # Plain `if`, not `(( … )) && …`: whether set -e fires on a false
        # arithmetic test ending an AND-list is too subtle to let it decide
        # whether a release aborts.
        if (( code > published_max )); then
            published_max=$code
            latest_name="$name"
        fi
    done
done
(( published_max > 0 )) || _warn "no published manifest readable — falling back to gradle.properties"

# max(local, published) + 1. gradle.properties counts even when nothing was
# published at that code, because a local build at it may already be sideloaded
# on a device — and reusing a code for different bytes leaves that device
# unable to ever see an update. Skipping a number costs nothing.
if [[ -z "$VERSION_CODE" ]]; then
    local_max=$(( CURRENT_CODE > published_max ? CURRENT_CODE : published_max ))
    VERSION_CODE=$(( local_max + 1 ))
    _info "versionCode $VERSION_CODE  (published max $published_max, gradle.properties $CURRENT_CODE)"
fi

# Prefill from what is actually live, not gradle.properties, and nudge the
# trailing number along — 0.2.0-beta2 offers 0.2.0-beta3. It is only a
# suggestion; vared leaves it fully editable.
if [[ -z "$VERSION_NAME" ]]; then
    suggestion="$latest_name"
    if [[ "$suggestion" =~ '^(.*[^0-9])([0-9]+)$' ]]; then
        suggestion="${match[1]}$(( match[2] + 1 ))"
    fi
    if [[ -t 0 ]]; then
        VERSION_NAME="$suggestion"
        print -r -- "  latest published: $latest_name (versionCode $published_max)"
        vared -p "  version name: " VERSION_NAME
    else
        VERSION_NAME="$suggestion"
        _info "version name: $suggestion (non-interactive)"
    fi
fi

VERSION_NAME="${VERSION_NAME## }"; VERSION_NAME="${VERSION_NAME%% }"
[[ -n "$VERSION_NAME" ]] || _die "version name is empty"
[[ "$VERSION_NAME" =~ '^[A-Za-z0-9][A-Za-z0-9._+-]*$' ]] \
    || _die "versionName '$VERSION_NAME' violates the client regex [A-Za-z0-9][A-Za-z0-9._+-]*"
(( VERSION_CODE > published_max )) \
    || _die "versionCode $VERSION_CODE does not exceed the published $published_max"

# Reusing a published name under a new code gives two different binaries one
# name, which is miserable to diagnose from a user's screenshot.
if (( ${published_names[(I)$VERSION_NAME]} )); then
    _warn "versionName '$VERSION_NAME' is already published (see above)."
    _confirm "Ship it again under versionCode $VERSION_CODE?"
fi

# Semver: anything after a hyphen is a prerelease. The release channel is where
# people who never opted into testing live.
if (( ${TARGETS[(I)release]} )) && [[ "$VERSION_NAME" == *-* ]]; then
    _warn "'$VERSION_NAME' looks like a prerelease, and release is the stable channel."
    _confirm "Publish a prerelease to the stable channel?"
fi

APK_NAME="promtuz-${VERSION_NAME}~${VERSION_CODE}.apk"
print -r -- ""
_info "version   $VERSION_NAME (versionCode $VERSION_CODE)"
_info "artefact  $APK_NAME"

# ── unlock ───────────────────────────────────────────────────────────────
_step "Unlock"

SCRATCH="$(mktemp -d)"; chmod 700 "$SCRATCH"
STAGE="$SCRATCH/stage"; mkdir -p "$STAGE"
trap 'rm -rf "$SCRATCH"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

_info "unlocking the vault…"
_unlock_vault "$SCRATCH/identity" "$VAULT/identity.age"
chmod 600 "$SCRATCH/identity"

age -d -i "$SCRATCH/identity" -o "$SCRATCH/release.p12"         "$VAULT/android-release.p12.age"
age -d -i "$SCRATCH/identity" -o "$SCRATCH/keystore.properties" "$VAULT/android-keystore.properties.age"
age -d -i "$SCRATCH/identity" -o "$SCRATCH/manifest.key"        "$VAULT/update-manifest.key.age"
chmod 600 "$SCRATCH"/*(.)

STORE_PASSWORD="$(sed -n 's/^storePassword=//p' "$SCRATCH/keystore.properties")"
KEY_ALIAS="$(sed -n 's/^keyAlias=//p' "$SCRATCH/keystore.properties")"
KEY_PASSWORD="$(sed -n 's/^keyPassword=//p' "$SCRATCH/keystore.properties")"
[[ -n "$STORE_PASSWORD" && -n "$KEY_ALIAS" && -n "$KEY_PASSWORD" ]] \
    || _die "keystore.properties in the vault is incomplete"

# Normalised to lowercase without colons so keytool's and apksigner's differing
# formats compare directly.
ks_listing="$(printf '%s\n' "$STORE_PASSWORD" \
    | "$JAVA_HOME/bin/keytool" -list -v -keystore "$SCRATCH/release.p12" 2>&1)" \
    || _die "keytool could not read the vault keystore:
$(print -r -- "$ks_listing" | sed 's/^/    /')"
ks_line="${${(f)ks_listing}[(r)*SHA256:*]}"
EXPECTED_SIGNER="${${${ks_line##*SHA256: }//:/}:l}"
[[ -n "$EXPECTED_SIGNER" ]] || _die "could not read the signer digest from the keystore"
_ok "signer $EXPECTED_SIGNER"

# The public half is what clients verify against, so prepare it once here and
# every signature produced here is checked against IT rather than against the
# private key that signed it — the check that catches a mismatched pair.
{ printf '302a300506032b6570032100'; cat "$VAULT/update-manifest.pub"; } | xxd -r -p > "$SCRATCH/mpub.der"
openssl pkey -pubin -inform DER -in "$SCRATCH/mpub.der" -out "$SCRATCH/mpub.pem" 2>/dev/null \
    || _die "vault's update-manifest.pub is not a valid Ed25519 public key"

# ── build each channel ───────────────────────────────────────────────────
sed -i.bak \
    -e "s/^promtuzVersionCode=.*/promtuzVersionCode=$VERSION_CODE/" \
    -e "s/^promtuzVersionName=.*/promtuzVersionName=$VERSION_NAME/" \
    "$GRADLE_PROPS"
rm -f "$GRADLE_PROPS.bak"

PUBLISHED_AT="$(date -u '+%Y-%m-%dT%H:%M:%SZ')"

for channel in $TARGETS; do
    _step "Build ${channel}"

    if [[ "$channel" == debug ]]; then
        # The debug channel ships genuinely debuggable builds — that is what
        # makes the client's nativeChannel resolve to "debug". A release-built
        # APK published here would report itself as release and the channel
        # logic would quietly disagree with reality.
        task=assembleDebug
        out_dir="$REPO/android/app/build/outputs/apk/debug"
    else
        task=assembleRelease
        out_dir="$REPO/android/app/build/outputs/apk/release"
    fi

    (
        cd "$REPO/android"
        export JAVA_HOME
        export PROMTUZ_ANDROID_KEYSTORE="$SCRATCH/release.p12"
        export PROMTUZ_ANDROID_STORE_PASSWORD="$STORE_PASSWORD"
        export PROMTUZ_ANDROID_KEY_ALIAS="$KEY_ALIAS"
        export PROMTUZ_ANDROID_KEY_PASSWORD="$KEY_PASSWORD"
        # gradlew is committed non-executable, so call the interpreter rather
        # than depend on a mode bit git does not carry here.
        sh gradlew --console=plain "$task"
    ) || _die "gradle $task failed"

    for abi in $ABIS; do
        # Match on the filename. AGP's split naming (app-<abi>-<variant>.apk)
        # has been stable far longer than the shape of output-metadata.json,
        # whose filter objects have changed keys between versions.
        matches=($out_dir/*${abi}*.apk(N))
        (( ${#matches} == 1 )) \
            || _die "expected exactly one $abi APK in $out_dir, found ${#matches} — is the ABI split config intact?"
        src="${matches[1]}"

        # One call, output captured, failure reported with apksigner's own words.
        # Piping it straight into awk would hide the reason: under pipefail a
        # non-zero apksigner takes the whole substitution down, and set -e then
        # ends the script in silence.
        certs="$("$APKSIGNER" verify --min-sdk-version 26 --print-certs "$src" 2>&1)" \
            || _die "apksigner rejected $channel/$abi:
$(print -r -- "$certs" | sed 's/^/    /')"

        line="${${(f)certs}[(r)*SHA-256 digest:*]}"
        signer="${${line##*: }:l}"
        [[ -n "$signer" ]] || _die "apksigner printed no certificate digest for $channel/$abi"
        [[ "$signer" == "$EXPECTED_SIGNER" ]] \
            || _die "$channel/$abi signed by $signer, expected $EXPECTED_SIGNER"

        dir="$STAGE/$channel/$abi"; mkdir -p "$dir"
        cp "$src" "$dir/$APK_NAME"

        sha="$(openssl dgst -sha256 "$dir/$APK_NAME" | awk '{print $NF}')"
        size="$(stat -f %z "$dir/$APK_NAME" 2>/dev/null || stat -c %s "$dir/$APK_NAME")"

        # EXACTLY the six fields UpdateManifest declares, in declaration order.
        cat > "$dir/manifest.json" <<EOF
{
  "versionCode": $VERSION_CODE,
  "versionName": "$VERSION_NAME",
  "apk": "$APK_NAME",
  "sha256": "$sha",
  "size": $size,
  "publishedAt": "$PUBLISHED_AT"
}
EOF

        # Sign the bytes on disk — the same bytes that get uploaded.
        openssl pkeyutl -sign -inkey "$SCRATCH/manifest.key" -rawin \
            -in "$dir/manifest.json" -out "$dir/manifest.json.sig" \
            || _die "could not sign the $channel/$abi manifest"
        sigsize="$(stat -f %z "$dir/manifest.json.sig" 2>/dev/null || stat -c %s "$dir/manifest.json.sig")"
        [[ "$sigsize" == 64 ]] || _die "manifest signature is $sigsize bytes, expected 64"

        openssl pkeyutl -verify -pubin -inkey "$SCRATCH/mpub.pem" -rawin \
            -in "$dir/manifest.json" -sigfile "$dir/manifest.json.sig" >/dev/null 2>&1 \
            || _die "$channel/$abi signature does not verify against the vault's public key"

        _ok "$channel/$abi  signed, ${size} bytes, sha256 ${sha:0:16}…"
    done
done

if (( ! PUBLISH )); then
    KEEP="$REPO/android/app/build/release-staging"
    rm -rf "$KEEP"; mkdir -p "$KEEP"
    cp -R "$STAGE"/* "$KEEP"/
    print -r -- ""
    _ok "staged at $KEEP (nothing published)"
    exit 0
fi

# ── publish ──────────────────────────────────────────────────────────────
_step "Publish"

for channel in $TARGETS; do
    for abi in $ABIS; do
        dest="$PUBLISH_ROOT/apk/$channel/$abi"
        _info "-> $PUBLISH_HOST:$dest"
        ssh "$PUBLISH_HOST" "mkdir -p '$dest'" || _die "could not create $dest"
        # APK first, manifest last: the manifest is what clients act on, so it
        # must never point at a file that is still uploading.
        rsync -q "$STAGE/$channel/$abi/$APK_NAME"         "$PUBLISH_HOST:$dest/" || _die "APK upload failed"
        rsync -q "$STAGE/$channel/$abi/manifest.json.sig" "$PUBLISH_HOST:$dest/" || _die "sig upload failed"
        rsync -q "$STAGE/$channel/$abi/manifest.json"     "$PUBLISH_HOST:$dest/" || _die "manifest upload failed"

        # A stable URL for manual downloads. Relative target so it resolves
        # inside $dest wherever the docroot moves to.
        #
        # The in-app updater never touches this: open() requires the path to
        # start with "promtuz-", so latest.apk is rejected as an invalid update
        # path. It serves manual downloads only, and nothing else maintains
        # it — so it goes stale unless updated here, invisibly.
        ssh "$PUBLISH_HOST" "ln -sfn '$APK_NAME' '$dest/latest.apk'" \
            || _die "could not update $dest/latest.apk"
        _ok "$channel/$abi uploaded (latest.apk -> $APK_NAME)"
    done
done

# ── confirm live ─────────────────────────────────────────────────────────
_step "Confirm live"

for channel in $TARGETS; do
    for abi in $ABIS; do
        url="$BASE_URL/apk/$channel/$abi"
        curl -fsS -m 30 "$url/manifest.json"     -o "$SCRATCH/live.json" || _die "$channel/$abi manifest unreachable"
        curl -fsS -m 30 "$url/manifest.json.sig" -o "$SCRATCH/live.sig"  || _die "$channel/$abi sig unreachable"

        cmp -s "$SCRATCH/live.json" "$STAGE/$channel/$abi/manifest.json" \
            || _die "$channel/$abi live manifest differs from what we uploaded"
        openssl pkeyutl -verify -pubin -inkey "$SCRATCH/mpub.pem" -rawin \
            -in "$SCRATCH/live.json" -sigfile "$SCRATCH/live.sig" >/dev/null 2>&1 \
            || _die "$channel/$abi live signature does not verify — clients would reject this"

        code="$(curl -fsS -m 30 -o /dev/null -w '%{http_code}' -r 0-0 "$url/$APK_NAME" || true)"
        [[ "$code" == 206 || "$code" == 200 ]] || _die "$channel/$abi APK unreachable (HTTP $code)"

        # Confirm latest.apk resolves to THIS build. Comparing Content-Length
        # against the manifest is what would have caught it going stale.
        want="$(sed -n 's/.*"size": \([0-9]*\).*/\1/p' "$STAGE/$channel/$abi/manifest.json")"
        got="$(curl -fsSI -m 30 "$url/latest.apk" 2>/dev/null | tr -d '\r' \
            | sed -n 's/^[Cc]ontent-[Ll]ength: //p')"
        [[ "$got" == "$want" ]] \
            || _die "$channel/$abi latest.apk is $got bytes, expected $want — the symlink did not update"

        _ok "$channel/$abi live, verifying, latest.apk current"
    done
done

print -r -- ""
_ok "published $VERSION_NAME (versionCode $VERSION_CODE) to ${TARGETS[*]}"

notify_failed=0
if (( NOTIFY )); then
    _step "Announce release"
    if ! python3 "${SCRIPT:h}/notify-android.py" --channel "$CHANNEL"; then
        notify_failed=1
        _warn "The release is published, but its announcement did not finish."
        _info "Retry without rebuilding: zsh ${(q)SCRIPT} --notify-only --channel $CHANNEL"
    fi
fi

print -r -- ""
print -r -- "Direct download links (stable, always the newest build):"
for channel in $TARGETS; do
    for abi in $ABIS; do
        print -r -- "  $abi  $BASE_URL/apk/$channel/$abi/latest.apk"
    done
done
print -r -- "  Most phones are arm64-v8a; x86_64 is emulators."

print -r -- ""
_info "commit the bump so the repo matches what shipped:"
_info "  git add android/gradle.properties && git commit -m 'chore: release $VERSION_NAME'"

(( notify_failed == 0 )) || exit 2
