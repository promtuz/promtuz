# Release tools

## Android

`release-android.zsh` builds both ABIs, signs the APKs and manifests, uploads them,
verifies the live files, and announces the release. `debug` builds are debuggable;
`release` builds are not. `--channel both` publishes one versionCode to both channels.

`--notes FILE` publishes a Markdown file as the release's notes, signed and stored as
`notes-<versionCode>.md` beside the APK. The app shows the notes of every version
between the installed and the offered build, skipping versions without any. A new
major or minor version makes the update required. Nothing about notes is kept in the
repo.

The build uses the signing vault at `~/.promtuz-vault` (`PZ_VAULT` overrides it).
Announcements also need Python 3 and an FCM service-account JSON for the app's
Firebase project. Keep that file outside the checkout:

```sh
export GOOGLE_APPLICATION_CREDENTIALS="$HOME/.config/promtuz/fcm-service-account.json"
zsh tools/scripts/release-android.zsh --channel release
```

The credential needs permission to send FCM messages. An existing gateway service
account for the same Firebase project can be used. The Android app never receives
this credential. Check the configuration without building or sending anything:

```sh
python3 tools/scripts/notify-android.py --check-credentials
```

Useful modes:

```sh
# Inspect the plan without network access, a vault, or an Android SDK.
zsh tools/scripts/release-android.zsh --channel both --dry-run

# Build and keep signed files in android/app/build/release-staging.
zsh tools/scripts/release-android.zsh --no-publish

# Publish with release notes.
zsh tools/scripts/release-android.zsh --channel release --notes ~/notes/0.5.0.md

# Publish without sending a release announcement.
zsh tools/scripts/release-android.zsh --channel release --no-notify

# Retry an announcement for the current live release without rebuilding.
zsh tools/scripts/release-android.zsh --channel release --notify-only
```

The release remains published if an announcement fails; the script exits with code
2 and prints the retry command. Retrying is safe: clients remember the last version
shown per channel. `--notify-only` checks both ABIs and verifies manifests against
the public key pinned in this checkout's core before sending anything.

Announcements are normal-priority FCM data messages to `promtuz-updates-debug` or
`promtuz-updates-release`. Clients fetch the signed manifest themselves, offer only
installable versions, and reuse one quiet notification. Delivery can be delayed;
a daily background check and the existing foreground check remain available.
Users can disable the **App updates** category in Android's notification settings.
Clients need the release-notification implementation installed before they can
receive these announcements.

`PZ_UPDATE_URL`, `PZ_PUBLISH_HOST`, and `PZ_PUBLISH_ROOT` override the release host
defaults. `--dry-run` does not reserve a versionCode; the real release chooses a code
above the local and published versions. Commit the resulting Gradle version bump
after publishing.

## Server packages

`publish-apt.zsh` builds and publishes the relay, resolver, and gateway Debian
packages. It accepts `--version`, `--channel`, `--no-publish`, and `--dry-run`.
Its publication does not send Android update notifications.
