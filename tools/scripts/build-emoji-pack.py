#!/usr/bin/env python3
"""Build the bundled emoji pack from an Apple Color Emoji TTF.

    tools/scripts/build-emoji-pack.py <AppleColorEmoji-Linux.ttf> <emoji-test.txt>

The font is the Linux build from github.com/samuelngs/apple-emoji-ttf, whose
bitmap strikes carry one PNG per glyph; emoji-test.txt is the Unicode list of
fully-qualified sequences the same release targets. Every sequence is shaped
with the font so its GSUB resolves joined sequences, skin tones, flags and
keycaps to one glyph, and that glyph's bitmap is re-encoded as lossy WebP.
Output lands in android/app/src/main/assets/emoji/ with an index the app
reads once. Keys are the sequence's codepoints in four-digit hex joined by
underscores, variation selectors dropped, which is how EmojiSequences names a
cluster on the device.

Needs ffmpeg with libwebp, and fonttools plus uharfbuzz on the Python path
(a venv with `pip install fonttools uharfbuzz` will do).
"""
from __future__ import annotations

import subprocess
import sys
import tempfile
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import uharfbuzz as hb
from fontTools.ttLib import TTFont

QUALITY = "85"
VARIATION_SELECTORS = {0xFE0E, 0xFE0F}


def key_of(cps: list[int]) -> str:
    return "_".join(f"{cp:04x}" for cp in cps if cp not in VARIATION_SELECTORS)


def sequences(listing: Path):
    for line in listing.read_text(encoding="utf-8").splitlines():
        body = line.split("#", 1)[0].strip()
        if not body or ";" not in body:
            continue
        cps, status = (part.strip() for part in body.split(";", 1))
        if status != "fully-qualified":
            continue
        yield [int(cp, 16) for cp in cps.split()]


def main() -> int:
    if len(sys.argv) != 3:
        print(__doc__, file=sys.stderr)
        return 2
    font_path, listing = Path(sys.argv[1]), Path(sys.argv[2])
    root = Path(subprocess.check_output(["git", "rev-parse", "--show-toplevel"], text=True).strip())
    out = root / "android/app/src/main/assets/emoji"
    out.mkdir(parents=True, exist_ok=True)
    for stale in out.iterdir():
        stale.unlink()

    tt = TTFont(font_path, lazy=True)
    names = tt.getGlyphOrder()
    strikes = tt["CBDT"].strikeData
    ppems = [s.bitmapSizeTable.ppemX for s in tt["CBLC"].strikes]
    strike = strikes[ppems.index(max(ppems))]
    font = hb.Font(hb.Face(hb.Blob.from_file_path(str(font_path))))

    def shape(cps: list[int]) -> list[int]:
        buf = hb.Buffer()
        buf.add_str("".join(chr(cp) for cp in cps))
        buf.guess_segment_properties()
        hb.shape(font, buf, {})
        return [g.codepoint for g in buf.glyph_infos]

    jobs: dict[str, bytes] = {}
    missing: list[str] = []
    for cps in sequences(listing):
        key = key_of(cps)
        if key in jobs:
            continue
        gids = shape(cps)
        glyph = strike.get(names[gids[0]]) if len(gids) == 1 and gids[0] != 0 else None
        if glyph is None:
            missing.append(key)
            continue
        glyph.decompile()
        jobs[key] = glyph.imageData

    def encode(item: tuple[str, bytes]) -> None:
        key, png = item
        with tempfile.NamedTemporaryFile(suffix=".png", delete=False) as tmp:
            tmp.write(png)
        try:
            subprocess.run(
                ["ffmpeg", "-loglevel", "error", "-y", "-i", tmp.name, "-c:v", "libwebp",
                 "-quality", QUALITY, "-pix_fmt", "yuva420p", str(out / f"{key}.webp")],
                check=True,
            )
        finally:
            Path(tmp.name).unlink(missing_ok=True)

    with ThreadPoolExecutor() as pool:
        list(pool.map(encode, jobs.items()))
    (out / "index.txt").write_text("\n".join(sorted(jobs)) + "\n", encoding="utf-8")

    total = sum(p.stat().st_size for p in out.glob("*.webp"))
    print(f"pack: {len(jobs)} glyphs at {max(ppems)}px, {total / 1_048_576:.1f} MB; "
          f"{len(missing)} listed sequences the font does not draw")
    return 0


if __name__ == "__main__":
    sys.exit(main())
