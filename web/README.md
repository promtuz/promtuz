# promtuz.dev deeplink assets

Static files that make `https://promtuz.dev/pair#<code>` open the app (Android
App Links, iOS Universal Links later). Deploy them so the paths line up:

| File | URL |
|------|-----|
| `.well-known/assetlinks.json` | `https://promtuz.dev/.well-known/assetlinks.json` |
| `pair/index.html`             | `https://promtuz.dev/pair`                        |

Serve `assetlinks.json` as `Content-Type: application/json` over HTTPS with no
redirect (Android fetches it verbatim to verify the App Link).

## Before it works

**Fingerprints** — put your app signing cert SHA-256(s) in `assetlinks.json`
(it takes a list — include both debug and release):
```
keytool -list -v -keystore ~/.android/debug.keystore \
  -alias androiddebugkey -storepass android | grep SHA256
```

## How a tap resolves

- App installed + `assetlinks.json` verified → the OS opens the app directly on
  `https://promtuz.dev/pair#<code>` (no browser); this page never loads.
- Not installed, or opened inside an in-app browser (WhatsApp/Discord webviews
  that bypass App Links) → this page loads and auto-fires an `intent://` URL to
  launch the app; if that fails it just shows fallback text.

The invite `<code>` lives in the URL `#fragment`, so it never reaches the
server logs — it's a bearer capability.
