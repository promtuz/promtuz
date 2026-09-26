#!/usr/bin/env python3
"""Bundle notices from the resolved Android and Rust dependencies without network requests."""

import argparse
import hashlib
import io
import json
from pathlib import Path
import re
import sys

if sys.version_info < (3, 11):
    sys.exit("License generation requires Python 3.11 or newer (for the standard-library TOML parser).")

import tomllib
import xml.etree.ElementTree as ET
import zipfile

ROOT = Path(__file__).resolve().parents[2]
NOTICES = Path(__file__).with_name("notices")
LICENSE_FILE = re.compile(r"^(licen[cs]e|copying|copyright|notice)([._-]|$)", re.I)
STANDARD = {name: (NOTICES / f"{name}.txt").read_text() for name in ("Apache-2.0", "MPL-2.0", "CC0-1.0")}


def archive_notices(path):
    notices = []

    def inspect(archive, prefix=""):
        with zipfile.ZipFile(archive) as contents:
            names = contents.namelist()
            for name in names:
                if LICENSE_FILE.match(Path(name).name) and not name.endswith("/"):
                    notices.append((prefix + name, contents.read(name).decode("utf-8", errors="replace")))
                elif name == "classes.jar" or (name.startswith("libs/") and name.endswith(".jar")):
                    inspect(io.BytesIO(contents.read(name)), name + "/")
            if "third_party_licenses.json" in names and "third_party_licenses.txt" in names:
                index = json.loads(contents.read("third_party_licenses.json"))
                text = contents.read("third_party_licenses.txt")
                for name, span in sorted(index.items()):
                    start, length = span["start"], span["length"]
                    if not (0 <= start <= len(text) and 0 <= length <= len(text) - start):
                        raise ValueError(f"Invalid third-party notice offsets in {path}: {name}")
                    notices.append((name, text[start:start + length].decode("utf-8")))

    inspect(path)
    return notices


def pom_metadata(path, poms, seen=None):
    if not path:
        return {}, []
    root = ET.parse(path).getroot()
    for node in root.iter():
        node.tag = node.tag.rsplit("}", 1)[-1]
    seen = (seen or set()) | {path}
    parent_id = ":".join(root.findtext("parent/" + part) or "" for part in ("groupId", "artifactId", "version"))
    parent_path = poms.get(parent_id)
    parent_info, parent_licenses = pom_metadata(parent_path, poms, seen) if parent_path and parent_path not in seen else ({}, [])
    info = {name: (root.findtext(name) or parent_info.get(name, "")).strip() for name in ("name", "url", "description")}
    licenses = [(node.findtext("name") or "", node.findtext("url") or "") for node in root.findall("licenses/license")]
    return info, licenses or parent_licenses


def android_libraries(inventory):
    for item in inventory["libraries"]:
        group, artifact, version = item["id"].split(":")
        info, licenses = pom_metadata(item.get("pom"), inventory["poms"])
        notices = archive_notices(item["artifact"])
        name = info.get("name", "")
        if not name or "${" in name:
            name = artifact
        labels = ["Apache-2.0" if "apache" in (label + url).lower() and "2" in label + url
                  else label for label, url in licenses if label]
        for label, url in licenses:
            if "apache" in (label + url).lower() and "2" in label + url:
                notices.append(("Apache License 2.0", STANDARD["Apache-2.0"]))
        if group == "org.aomedia.avif.android" and artifact == "avif":
            if version != "1.3.0.841110fd":
                raise ValueError("Review tools/licenses/notices/libavif.txt and dav1d.txt for the new AVIF version")
            name = "libavif · dav1d"
            labels = ["BSD-2-Clause"]
            info["url"] = "https://github.com/AOMediaCodec/libavif"
            notices += [("libavif and bundled notices", (NOTICES / "libavif.txt").read_text()),
                        ("dav1d", (NOTICES / "dav1d.txt").read_text()),
                        ("Android CPU features", (NOTICES / "android-cpufeatures.txt").read_text())]
        elif group == "net.java.dev.jna" and artifact == "jna":
            if version != "5.19.0":
                raise ValueError("Review tools/licenses/notices/libffi.txt for the new JNA version")
            name = "Java Native Access"
            labels = ["LGPL-2.1-or-later OR Apache-2.0"]
            info["url"] = "https://github.com/java-native-access/jna"
            notices.append(("Apache License 2.0", STANDARD["Apache-2.0"]))
            notices.append(("libffi", (NOTICES / "libffi.txt").read_text()))
        supplements = {
            "dev.shreyaspatil:capturable:3.0.1": "capturable.txt",
            "androidx.datastore:datastore-preferences-external-protobuf:1.1.7": "protobuf.txt",
        }
        if item["id"] in supplements:
            notices.append(("License", (NOTICES / supplements[item["id"]]).read_text()))
        if any("mozilla.org/MPL/2.0" in text or "mozilla.org/en-US/MPL/2.0" in text for _, text in notices):
            notices.append(("Mozilla Public License 2.0", STANDARD["MPL-2.0"]))
        yield dict(id=item["id"], name=name, version=version,
                   url=info.get("url", ""), license=" · ".join(labels), notices=notices)


def package_field(package, field, directory):
    value = package.get(field, "")
    if not isinstance(value, dict):
        return value, directory
    if value.get("workspace") is not True:
        raise ValueError(f"Unsupported package.{field} in {directory / 'Cargo.toml'}")
    roots = [directory / package["workspace"]] if package.get("workspace") else [directory, *directory.parents]
    for root in roots:
        manifest = root / "Cargo.toml"
        if manifest.is_file():
            workspace = tomllib.loads(manifest.read_text()).get("workspace", {})
            if field in workspace.get("package", {}):
                return workspace["package"][field], root
    raise ValueError(f"Cannot resolve inherited package.{field} in {directory / 'Cargo.toml'}")


def rust_libraries(artifacts):
    manifests = set()
    targets = {"aarch64-linux-android", "x86_64-linux-android"}
    built_targets = set()
    for line in Path(artifacts).read_text().splitlines():
        message = json.loads(line)
        if message.get("reason") != "compiler-artifact":
            continue
        platforms = set().union(*(set(Path(p).parts) & targets for p in message["filenames"]))
        if not platforms:
            continue  # Host-only procedural macros and build tools are not shipped.
        built_targets.update(platforms)
        manifests.add(Path(message["manifest_path"]))
    if built_targets != targets:
        raise ValueError("Rust license inventory needs buildRustCore artifacts for both Android targets")
    for manifest in sorted(manifests):
        directory = manifest.parent
        if directory in (ROOT / "libcore", ROOT / "common"):
            continue
        package = tomllib.loads(manifest.read_text())["package"]
        version, _ = package_field(package, "version", directory)
        expression, _ = package_field(package, "license", directory)
        repository, _ = package_field(package, "repository", directory)
        notices = []
        for path in sorted(directory.rglob("*")):
            if path.is_file() and LICENSE_FILE.match(path.name):
                # sqlcipher is an alternative backend, not our bundled SQLite build.
                if package["name"] == "libsqlite3-sys" and "sqlcipher" in path.parts:
                    continue
                notices.append((str(path.relative_to(directory)), path.read_text(errors="replace")))
        if package.get("license-file"):
            license_file, license_root = package_field(package, "license-file", directory)
            if not any(title == license_file for title, _ in notices):
                notices.append((license_file, (license_root / license_file).read_text()))
        for name, text in STANDARD.items():
            if name in expression and not notices:
                notices.append((name, text))
                break
        openmls_versions = {"openmls": "0.8.1", "openmls_basic_credential": "0.5.0",
                           "openmls_memory_storage": "0.5.0", "openmls_rust_crypto": "0.5.1",
                           "openmls_traits": "0.5.0"}
        if openmls_versions.get(package["name"]) == version and not notices:
            notices.append(("MIT License", (NOTICES / "openmls.txt").read_text()))
        if package["name"] == "libsqlite3-sys":
            source = (directory / "sqlite3/sqlite3.c").read_text()
            notices.append(("SQLite", source[:source.index("*/") + 2]))
        yield dict(id=f"crate:{package['name']}:{version}", name=package["name"],
                   version=version, url=repository,
                   license=expression, notices=notices)


def bundled_assets():
    """Bundled artwork and data that are not pulled in as dependencies."""
    yield dict(id="data:unicode-emoji", name="Unicode Emoji Data", version="17.0",
               url="https://www.unicode.org/Public/17.0.0/ucd/emoji/emoji-data.txt", license="Unicode-3.0",
               notices=[("Unicode Emoji Data", (NOTICES / "unicode.txt").read_text())])
    yield dict(id="assets:apple-color-emoji", name="Apple Color Emoji", version="macOS 26 (20260722)",
               url="https://github.com/samuelngs/apple-emoji-ttf", license="Apple Inc. artwork · MIT (build tooling)",
               notices=[("Apple Color Emoji", (NOTICES / "apple-color-emoji.txt").read_text()),
                        ("apple-emoji-ttf", (NOTICES / "apple-emoji-ttf.txt").read_text())])


def generate(args):
    output = Path(args.output) / "licenses"
    output.mkdir(parents=True, exist_ok=True)
    entries = []
    missing = []
    for library in [*android_libraries(json.loads(Path(args.android).read_text())), *rust_libraries(args.rust_artifacts),
                    *bundled_assets()]:
        if not library["notices"]:
            missing.append(library["id"])
            continue
        identifier = hashlib.sha256(library["id"].encode()).hexdigest()[:24]
        references = []
        seen = set()
        for title, text in library.pop("notices"):
            text = text.strip() + "\n"
            digest = hashlib.sha256(text.encode()).hexdigest()
            if digest in seen:
                continue
            seen.add(digest)
            (output / f"{digest}.txt").write_text(text)
            references.append(dict(title=title, file=f"licenses/{digest}.txt"))
        library["notices"] = references
        # Some artifacts publish their terms inside the archive rather than a POM.
        library["license"] = library["license"] or "License notices"
        (output / f"{identifier}.json").write_text(json.dumps(library, ensure_ascii=False))
        entries.append(dict(id=identifier, coordinate=library["id"], name=library["name"], version=library["version"], license=library["license"]))
    if missing:
        raise ValueError("Missing license notices for:\n  " + "\n  ".join(missing) +
                         "\nAdd verified upstream notices or fix the published POM metadata in tools/licenses/generate.py.")
    entries.sort(key=lambda item: (item["name"].casefold(), item["version"]))
    (output / "index.json").write_text(json.dumps(entries, ensure_ascii=False))
    expected = {"index.json"} | {f"{entry['id']}.json" for entry in entries}
    for entry in entries:
        expected.update(Path(n["file"]).name for n in json.loads((output / f"{entry['id']}.json").read_text())["notices"])
    for file in output.iterdir():
        if file.name not in expected:
            file.unlink()
    print(f"Bundled license notices for {len(entries)} dependencies")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--android", required=True)
    parser.add_argument("--output", required=True)
    parser.add_argument("--rust-artifacts", required=True)
    args = parser.parse_args()
    try:
        generate(args)
    except (OSError, ValueError, zipfile.BadZipFile) as error:
        sys.exit(str(error))


if __name__ == "__main__":
    main()
