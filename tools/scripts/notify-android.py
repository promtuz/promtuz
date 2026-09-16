#!/usr/bin/env python3
"""Verify a published Android release, then send an FCM update-check hint."""

import argparse
import base64
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.parse
import urllib.request


REPO = Path(__file__).resolve().parents[2]
ABIS = ("arm64-v8a", "x86_64")
TOKEN_URL = "https://oauth2.googleapis.com/token"


class ReleaseError(Exception):
    pass


class NoRedirect(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


HTTP = urllib.request.build_opener(NoRedirect)


def request(url, *, data=None, headers=None, limit=16_384):
    req = urllib.request.Request(url, data=data, headers=headers or {})
    try:
        with HTTP.open(req, timeout=30) as response:
            body = response.read(limit + 1)
            if len(body) > limit:
                raise ReleaseError(f"Response too large from {urllib.parse.urlsplit(url).hostname}")
            return body
    except urllib.error.HTTPError as error:
        # OAuth responses and request headers can contain credentials.
        error.close()
        raise ReleaseError(f"{urllib.parse.urlsplit(url).hostname}: HTTP {error.code}") from None
    except urllib.error.URLError as error:
        raise ReleaseError(f"{urllib.parse.urlsplit(url).hostname}: {error.reason}") from None


def openssl(*args, data=None):
    result = subprocess.run(["openssl", *args], input=data, capture_output=True)
    if result.returncode:
        raise ReleaseError(f"OpenSSL {args[0]} failed")
    return result.stdout


def pinned_public_key():
    source = (REPO / "libcore/src/api/update.rs").read_text()
    constant = re.search(r"const UPDATE_MANIFEST_PUBLIC_KEY:.*?=\s*\[(.*?)\];", source, re.S)
    if not constant:
        raise ReleaseError("Cannot find the app's update verification key")
    key = bytes(int(value, 16) for value in re.findall(r"0x([0-9a-fA-F]{2})", constant[1]))
    if len(key) != 32:
        raise ReleaseError("The app's update verification key must be 32 bytes")
    return bytes.fromhex("302a300506032b6570032100") + key


def verify_release(base_url, channel, directory):
    versions = []
    public_key = directory / "manifest.pub.der"
    public_key.write_bytes(pinned_public_key())
    for abi in ABIS:
        url = f"{base_url}/apk/{channel}/{abi}"
        raw = request(f"{url}/manifest.json")
        signature = request(f"{url}/manifest.json.sig", limit=64)
        (directory / "manifest.json").write_bytes(raw)
        (directory / "manifest.sig").write_bytes(signature)
        try:
            openssl("pkeyutl", "-verify", "-pubin", "-keyform", "DER", "-inkey", str(public_key),
                    "-rawin", "-in", str(directory / "manifest.json"),
                    "-sigfile", str(directory / "manifest.sig"))
        except ReleaseError:
            raise ReleaseError(f"{channel}/{abi}: manifest signature does not match the app's key") from None
        manifest = json.loads(raw)
        fields = {"versionCode", "versionName", "apk", "sha256", "size", "publishedAt"}
        if not isinstance(manifest, dict) or set(manifest) != fields:
            raise ReleaseError(f"{channel}/{abi}: unexpected manifest fields")
        code, name, size = manifest["versionCode"], manifest["versionName"], manifest["size"]
        if type(code) is not int or not 0 < code <= 2_100_000_000:
            raise ReleaseError(f"{channel}/{abi}: invalid versionCode")
        if not isinstance(name, str) or not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._+-]*", name):
            raise ReleaseError(f"{channel}/{abi}: invalid versionName")
        if manifest["apk"] != f"promtuz-{name}~{code}.apk":
            raise ReleaseError(f"{channel}/{abi}: unexpected APK filename")
        if type(size) is not int or size <= 0 or not isinstance(manifest["sha256"], str) or \
                not re.fullmatch(r"[0-9a-f]{64}", manifest["sha256"]):
            raise ReleaseError(f"{channel}/{abi}: invalid APK size or hash")
        if not isinstance(manifest["publishedAt"], str):
            raise ReleaseError(f"{channel}/{abi}: invalid publication date")

        req = urllib.request.Request(f"{url}/{manifest['apk']}", headers={"Range": "bytes=0-0"})
        try:
            with HTTP.open(req, timeout=30) as response:
                available = (response.status == 206 and
                             response.headers.get("Content-Range") == f"bytes 0-0/{size}") or \
                            (response.status == 200 and response.headers.get("Content-Length") == str(size))
                if not available or not response.read(1):
                    raise ReleaseError(f"{channel}/{abi}: published APK is missing or has the wrong size")
        except urllib.error.HTTPError as error:
            error.close()
            raise ReleaseError(f"{channel}/{abi}: APK unavailable (HTTP {error.code})") from None
        except urllib.error.URLError as error:
            raise ReleaseError(f"{channel}/{abi}: APK unavailable ({error.reason})") from None
        versions.append((code, name))
        print(f"  Verified {channel}/{abi}: {name} ({code})", flush=True)
    if len(set(versions)) != 1:
        raise ReleaseError(f"{channel}: ABIs have different versions; finish publishing before notifying")
    return versions[0]


def credentials(path):
    if not path:
        raise ReleaseError("Set GOOGLE_APPLICATION_CREDENTIALS to the FCM service-account JSON, or publish with --no-notify")
    try:
        account = json.loads(Path(path).expanduser().read_text())
        if account.get("type") != "service_account" or not all(
            isinstance(account.get(key), str) and account[key]
            for key in ("project_id", "client_email", "private_key")
        ):
            raise ValueError()
    except (OSError, ValueError, AttributeError):
        raise ReleaseError("Cannot read a valid FCM service-account JSON from the configured path") from None
    app_project = json.loads((REPO / "android/app/google-services.json").read_text())["project_info"]["project_id"]
    if account["project_id"] != app_project:
        raise ReleaseError(f"FCM credentials must belong to the app's Firebase project: {app_project}")
    return account


def access_token(account, directory):
    def encode(value):
        return base64.urlsafe_b64encode(value).rstrip(b"=")

    now = int(time.time())
    header = {"alg": "RS256", "typ": "JWT"}
    claims = {"iss": account["client_email"], "aud": TOKEN_URL, "iat": now, "exp": now + 3600,
              "scope": "https://www.googleapis.com/auth/firebase.messaging"}
    unsigned = b".".join(encode(json.dumps(item, separators=(",", ":")).encode()) for item in (header, claims))
    key = directory / "fcm.pem"
    key.touch(mode=0o600)
    key.write_text(account["private_key"])
    try:
        signature = openssl("dgst", "-sha256", "-sign", str(key), data=unsigned)
    finally:
        key.unlink()
    assertion = (unsigned + b"." + encode(signature)).decode()
    response = json.loads(request(TOKEN_URL, data=urllib.parse.urlencode({
        "grant_type": "urn:ietf:params:oauth:grant-type:jwt-bearer", "assertion": assertion,
    }).encode(), headers={"Content-Type": "application/x-www-form-urlencoded"}))
    token = response.get("access_token") if isinstance(response, dict) else None
    if not isinstance(token, str) or not token:
        raise ReleaseError("OAuth did not return an access token")
    return token


def announce(account, token, channel, version):
    code, name = version
    body = {"message": {
        "topic": f"promtuz-updates-{channel}",
        "android": {"priority": "normal", "collapse_key": f"app-update-{channel}", "ttl": "86400s"},
        "data": {"type": "app_update", "channel": channel, "versionCode": str(code)},
    }}
    result = json.loads(request(
        f"https://fcm.googleapis.com/v1/projects/{account['project_id']}/messages:send",
        data=json.dumps(body).encode(),
        headers={"Content-Type": "application/json", "Authorization": f"Bearer {token}"},
    ))
    if not isinstance(result, dict) or not result.get("name"):
        raise ReleaseError("FCM did not acknowledge the announcement")
    print(f"  Announced {channel}: {name} ({code})", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--channel", choices=("debug", "release", "both"), default="debug")
    parser.add_argument("--service-account", default=os.environ.get("GOOGLE_APPLICATION_CREDENTIALS"))
    parser.add_argument("--base-url", default=os.environ.get("PZ_UPDATE_URL", "https://apt.promtuz.dev"))
    parser.add_argument("--check-credentials", action="store_true", help="Validate configuration without sending anything")
    parser.add_argument("--dry-run", action="store_true", help="Print the plan without network access or credentials")
    args = parser.parse_args()
    channels = ("release", "debug") if args.channel == "both" else (args.channel,)
    if args.dry_run:
        for channel in channels:
            print(f"Would verify both ABIs at {args.base_url}/apk/{channel}, then announce to promtuz-updates-{channel}")
        return
    account = credentials(args.service_account)
    if args.check_credentials:
        openssl("rsa", "-check", "-noout", data=account["private_key"].encode())
        print(f"  FCM project: {account['project_id']}")
        return
    base = args.base_url.rstrip("/")
    parsed = urllib.parse.urlsplit(base)
    if parsed.scheme != "https" or not parsed.hostname or parsed.username or parsed.password or parsed.query or parsed.fragment:
        raise ReleaseError("The update base URL must use HTTPS without credentials, query, or fragment")
    with tempfile.TemporaryDirectory(prefix="promtuz-notify-") as scratch:
        directory = Path(scratch)
        releases = {channel: verify_release(base, channel, directory) for channel in channels}
        token = access_token(account, directory)
        for channel, version in releases.items():
            announce(account, token, channel, version)


if __name__ == "__main__":
    try:
        main()
    except (ReleaseError, OSError, ValueError, KeyError) as error:
        print(f"Announcement failed: {error}", file=sys.stderr)
        sys.exit(1)
    except KeyboardInterrupt:
        sys.exit(130)
