#!/usr/bin/env python3
"""Refuse release APKs whose arm64 native libraries cannot load on 16 KiB pages."""

import argparse
import struct
import sys
import zipfile


PAGE_SIZE = 16 * 1024
ARM64_PREFIX = "lib/arm64-v8a/"


def local_data_offset(archive: zipfile.ZipFile, entry: zipfile.ZipInfo) -> int:
    archive.fp.seek(entry.header_offset)
    header = archive.fp.read(30)
    if len(header) != 30 or header[:4] != b"PK\x03\x04":
        raise ValueError(f"{entry.filename}: invalid local ZIP header")
    name_length, extra_length = struct.unpack_from("<HH", header, 26)
    return entry.header_offset + 30 + name_length + extra_length


def verify_elf(name: str, blob: bytes) -> None:
    if blob[:6] != b"\x7fELF\x02\x01":
        raise ValueError(f"{name}: expected a 64-bit little-endian ELF")
    program_offset = struct.unpack_from("<Q", blob, 32)[0]
    entry_size, count = struct.unpack_from("<HH", blob, 54)
    if entry_size < 56:
        raise ValueError(f"{name}: invalid ELF program-header size")
    for index in range(count):
        offset = program_offset + index * entry_size
        if offset + 56 > len(blob):
            raise ValueError(f"{name}: truncated ELF program headers")
        segment_type = struct.unpack_from("<I", blob, offset)[0]
        alignment = struct.unpack_from("<Q", blob, offset + 48)[0]
        if segment_type == 1 and alignment < PAGE_SIZE:
            raise ValueError(f"{name}: PT_LOAD alignment {alignment:#x} is below {PAGE_SIZE:#x}")


def verify(apk: str) -> None:
    with zipfile.ZipFile(apk) as archive:
        libraries = [entry for entry in archive.infolist() if entry.filename.startswith(ARM64_PREFIX) and entry.filename.endswith(".so")]
        if not libraries:
            raise ValueError(f"{apk}: no arm64 native libraries")
        for entry in libraries:
            if entry.compress_type != zipfile.ZIP_STORED:
                raise ValueError(f"{entry.filename}: native library is compressed")
            if local_data_offset(archive, entry) % PAGE_SIZE:
                raise ValueError(f"{entry.filename}: ZIP payload is not {PAGE_SIZE // 1024} KiB aligned")
            verify_elf(entry.filename, archive.read(entry))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("apk")
    try:
        verify(parser.parse_args().apk)
    except (OSError, ValueError, zipfile.BadZipFile) as error:
        print(f"16 KiB verification failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
