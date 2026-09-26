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

## Release notes

`--notes FILE` takes one Markdown file holding the body of that release's notes.
The script writes the `# <version> - <date>` first line itself, so never put a
version number or a date in the file. Keep it under 64 KB; the script refuses an
empty or a larger one.

The app shows the notes of every release between the installed and the offered
build, newest first, up to ten, each under its own version chip and date.

### What the app renders

The renderer is deliberately small. It reads the file line by line, trims each
line, and drops the blank ones.

| Write | You get |
| --- | --- |
| `### Calls` | A section heading. Every `#` depth looks the same, so use one level. |
| `- Audio calls` | A bullet. `* ` works too. |
| Any other line | A paragraph. |
| `**no direct path**` | Bold. |

Nothing else is Markdown. Links, italics, code spans, block quotes, numbered lists
and horizontal rules all reach the screen as their literal characters, the brackets
and the URL of `[text](url)` included.

Two consequences are worth remembering:

- **One paragraph per line.** Every source line is its own block, so a sentence
  wrapped across two lines arrives as two paragraphs. Let the lines run long.
- **No nesting.** Indentation is trimmed away, so an indented bullet becomes an
  ordinary one. Use a heading where you want a sub-list.

Blank lines cost nothing and do not change the spacing, so use them to keep the
file readable.

### Writing them from the commits

The commit log is the input, not the output. Commits are written for whoever
maintains the code; these notes are read by someone deciding whether to install.
Walk the log since the last release and keep only what a person would notice on
their own screen.

Do:

- Group the entries under a few headings that name an area: Calls, Media, Fixes.
- Say what the person can now do, in their words, one line each.
- Lead with the change that matters most and keep the list short.
- Fold several commits into the single sentence that describes their result.
- Say what a fix means for them, not what was wrong in the code.

Don't:

- Don't paste commit subjects, and never their `feat(android):` prefixes.
- Don't list refactors, build changes, dependency bumps or test work. Anything
  with no visible effect is better left out than padded in.
- Don't name files, functions, modules or protocol versions.
- Don't credit contributors with `@handles` or point at issue numbers. Neither
  one renders, and both read as noise to everyone else.
- Don't give a line to a bug that never reached a published build. Check whether
  the last release actually had it; if it did not, the fix belongs to whichever
  feature introduced it.
- Don't promise anything this build does not ship.

### Example

Given this log:

```
feat(web): add contact-card link fallback
feat(android): receive shared text and media from other apps
feat(android): add contact profiles, requests and shared media
refactor(android): unify dialogs and refine modal presentation
style(android): use outlined QR code and scanner icons
feat(core): add signed contact requests and synchronized profiles
fix(core): exclude deleted messages from visible history
```

the file holds:

```markdown
### Contacts
- Everyone you chat with now has a profile, with their picture and the media you have shared.
- Adding someone sends them a request, so nobody lands in your chats uninvited.

### Sharing
- Send text, photos and files to Promtuz straight from any other app.

### Fixes
- Deleted messages no longer come back when a chat reloads its history.
```

The web commit is not this app. The dialog refactor and the icon change are
invisible to anyone who was not already looking for them. The two contact commits
and the core one behind them are a single feature from the outside, so they earn a
section rather than three lines.

## Server packages

`publish-apt.zsh` builds and publishes the relay, resolver, and gateway Debian
packages. It accepts `--version`, `--channel`, `--no-publish`, and `--dry-run`.
Its publication does not send Android update notifications.
