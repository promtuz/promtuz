#!/usr/bin/env python3
"""
SVG -> Android VectorDrawable, in this project's icon house style.

The point is to skip the export-a-file/import-one-by-one dance: select the icon
in Figma, Copy as SVG (cmd-shift-C), then

    tools/scripts/svg2vd.py oi_mute

and app/src/main/res/drawable/oi_mute.xml exists. Batch a folder with --from.

Names are taken as given — the drawable folder mixes i_, oi_, ic_ and logo_, so
nothing is assumed about which family an icon belongs to. Pass --prefix when a
whole batch shares one.

Only the SVG subset icons actually use is handled — shapes, groups, transforms,
fills and strokes. Anything a VectorDrawable genuinely cannot express (gradients,
masks, dash patterns) is reported on stderr rather than silently dropped, so a
wrong icon never lands in the tree unnoticed.

Stdlib only; no install step.
"""

from __future__ import annotations

import argparse
import math
import os
import re
import subprocess
import sys
import xml.etree.ElementTree as ET

# Matches every existing oi_*.xml. The value is irrelevant at runtime — Compose's
# Icon tints the whole drawable — but staying consistent keeps diffs quiet.
HOUSE_COLOR = "#e3e3e3"
SVG_NS = "http://www.w3.org/2000/svg"

# --- geometry ---------------------------------------------------------------

# An affine transform as SVG orders it: x' = a*x + c*y + e, y' = b*x + d*y + f.
IDENTITY = (1.0, 0.0, 0.0, 1.0, 0.0, 0.0)


def mat_mul(m: tuple, n: tuple) -> tuple:
    """m applied after n (i.e. the SVG nesting order, outer * inner)."""
    a1, b1, c1, d1, e1, f1 = m
    a2, b2, c2, d2, e2, f2 = n
    return (
        a1 * a2 + c1 * b2,
        b1 * a2 + d1 * b2,
        a1 * c2 + c1 * d2,
        b1 * c2 + d1 * d2,
        a1 * e2 + c1 * f2 + e1,
        b1 * e2 + d1 * f2 + f1,
    )


def apply(m: tuple, x: float, y: float) -> tuple:
    a, b, c, d, e, f = m
    return (a * x + c * y + e, b * x + d * y + f)


def apply_vec(m: tuple, x: float, y: float) -> tuple:
    """Transform a direction — translation excluded."""
    a, b, c, d, _, _ = m
    return (a * x + c * y, b * x + d * y)


NUM = r"[+-]?(?:\d*\.\d+|\d+\.?)(?:[eE][+-]?\d+)?"


def parse_transform(spec: str) -> tuple:
    """translate/scale/rotate/matrix/skewX/skewY, composed left to right."""
    out = IDENTITY
    for name, args in re.findall(r"(\w+)\s*\(([^)]*)\)", spec or ""):
        v = [float(n) for n in re.findall(NUM, args)]
        if name == "translate":
            m = (1, 0, 0, 1, v[0], v[1] if len(v) > 1 else 0)
        elif name == "scale":
            sx = v[0]
            m = (sx, 0, 0, v[1] if len(v) > 1 else sx, 0, 0)
        elif name == "rotate":
            r = math.radians(v[0])
            cos, sin = math.cos(r), math.sin(r)
            m = (cos, sin, -sin, cos, 0, 0)
            if len(v) == 3:  # rotate about a point
                m = mat_mul(mat_mul((1, 0, 0, 1, v[1], v[2]), m), (1, 0, 0, 1, -v[1], -v[2]))
        elif name == "matrix":
            m = tuple(v[:6])
        elif name == "skewX":
            m = (1, 0, math.tan(math.radians(v[0])), 1, 0, 0)
        elif name == "skewY":
            m = (1, math.tan(math.radians(v[0])), 0, 1, 0, 0)
        else:
            continue
        out = mat_mul(out, m)
    return out


def fmt(n: float) -> str:
    """Compact number: no trailing zeros, no scientific notation."""
    if n == int(n) and abs(n) < 1e15:
        return str(int(n))
    s = f"{n:.5f}".rstrip("0").rstrip(".")
    return s if s not in ("-0", "") else "0"


# --- path data --------------------------------------------------------------

PARAM_COUNT = {
    "M": 2, "L": 2, "H": 1, "V": 1, "C": 6, "S": 4, "Q": 4, "T": 2, "A": 7, "Z": 0
}


def tokenize_path(d: str):
    """Yield (command, [floats]) with implicit repeats expanded."""
    for cmd, chunk in re.findall(r"([MmLlHhVvCcSsQqTtAaZz])([^MmLlHhVvCcSsQqTtAaZz]*)", d or ""):
        nums = [float(n) for n in re.findall(NUM, chunk)]
        n = PARAM_COUNT[cmd.upper()]
        if n == 0:
            yield (cmd, [])
            continue
        if not nums:
            continue
        for i in range(0, len(nums) - n + 1, n):
            group = nums[i:i + n]
            yield (cmd, group)
            # A repeated moveto means lineto for every pair after the first.
            if cmd == "M":
                cmd = "L"
            elif cmd == "m":
                cmd = "l"


def transform_path(d: str, m: tuple, warn) -> str:
    """Rewrite path data through m, converting everything to absolute commands."""
    out = []
    cx = cy = 0.0          # current point, user space
    sx = sy = 0.0          # subpath start
    det = m[0] * m[3] - m[1] * m[2]
    # Scale factors along each axis; equal for a similarity transform.
    scale_x = math.hypot(m[0], m[1])
    scale_y = math.hypot(m[2], m[3])
    rotation = math.degrees(math.atan2(m[1], m[0]))

    def emit(cmd, pts):
        out.append(cmd + " ".join(fmt(v) for v in pts))

    for cmd, v in tokenize_path(d):
        up = cmd.upper()
        rel = cmd.islower()

        if up == "Z":
            out.append("Z")
            cx, cy = sx, sy
            continue

        # Normalise to absolute user-space coordinates first.
        if up == "H":
            nx, ny = (cx + v[0] if rel else v[0]), cy
            up = "L"
        elif up == "V":
            nx, ny = cx, (cy + v[0] if rel else v[0])
            up = "L"
        elif up == "A":
            rx, ry, rot, laf, sf, ex, ey = v
            nx, ny = (cx + ex, cy + ey) if rel else (ex, ey)
            if abs(scale_x - scale_y) > 1e-6 and abs(rot) > 1e-6:
                warn("arc with non-uniform scale and x-rotation — shape may be off")
            tx, ty = apply(m, nx, ny)
            # Mirroring flips the sweep direction.
            emit("A", [rx * scale_x, ry * scale_y, rot + rotation,
                       int(laf), int(sf) if det >= 0 else 1 - int(sf), tx, ty])
            cx, cy = nx, ny
            continue
        else:
            pts = []
            for i in range(0, len(v), 2):
                px, py = v[i], v[i + 1]
                pts.append((cx + px, cy + py) if rel else (px, py))
            nx, ny = pts[-1]

        if up == "L":
            tx, ty = apply(m, nx, ny)
            emit("L", [tx, ty])
        elif up == "M":
            tx, ty = apply(m, nx, ny)
            emit("M", [tx, ty])
            sx, sy = nx, ny
        else:
            flat = []
            for px, py in pts:
                flat.extend(apply(m, px, py))
            emit(up, flat)

        cx, cy = nx, ny

    return " ".join(out).replace(" -", "-").strip()


# --- shapes -> path data ----------------------------------------------------


def shape_to_path(tag: str, at: dict, warn) -> str | None:
    """Convert a primitive shape element to equivalent path data."""
    g = lambda k, dflt=0.0: float(at.get(k, dflt) or 0)

    if tag == "path":
        return at.get("d")

    if tag == "rect":
        x, y, w, h = g("x"), g("y"), g("width"), g("height")
        if w <= 0 or h <= 0:
            return None
        rx = at.get("rx")
        ry = at.get("ry")
        rx = float(rx) if rx is not None else (float(ry) if ry is not None else 0.0)
        ry = float(ry) if ry is not None else rx
        rx, ry = min(rx, w / 2), min(ry, h / 2)
        if rx <= 0 or ry <= 0:
            return f"M{fmt(x)} {fmt(y)}H{fmt(x+w)}V{fmt(y+h)}H{fmt(x)}Z"
        return (
            f"M{fmt(x+rx)} {fmt(y)}"
            f"H{fmt(x+w-rx)}A{fmt(rx)} {fmt(ry)} 0 0 1 {fmt(x+w)} {fmt(y+ry)}"
            f"V{fmt(y+h-ry)}A{fmt(rx)} {fmt(ry)} 0 0 1 {fmt(x+w-rx)} {fmt(y+h)}"
            f"H{fmt(x+rx)}A{fmt(rx)} {fmt(ry)} 0 0 1 {fmt(x)} {fmt(y+h-ry)}"
            f"V{fmt(y+ry)}A{fmt(rx)} {fmt(ry)} 0 0 1 {fmt(x+rx)} {fmt(y)}Z"
        )

    if tag in ("circle", "ellipse"):
        cx, cy = g("cx"), g("cy")
        rx = g("r") if tag == "circle" else g("rx")
        ry = rx if tag == "circle" else g("ry")
        if rx <= 0 or ry <= 0:
            return None
        # Two half-arcs; a single 360° arc is degenerate.
        return (
            f"M{fmt(cx-rx)} {fmt(cy)}"
            f"A{fmt(rx)} {fmt(ry)} 0 1 0 {fmt(cx+rx)} {fmt(cy)}"
            f"A{fmt(rx)} {fmt(ry)} 0 1 0 {fmt(cx-rx)} {fmt(cy)}Z"
        )

    if tag == "line":
        return f"M{fmt(g('x1'))} {fmt(g('y1'))}L{fmt(g('x2'))} {fmt(g('y2'))}"

    if tag in ("polyline", "polygon"):
        nums = [float(n) for n in re.findall(NUM, at.get("points", ""))]
        if len(nums) < 4:
            return None
        pts = [f"{fmt(nums[i])} {fmt(nums[i+1])}" for i in range(0, len(nums) - 1, 2)]
        d = "M" + "L".join(pts)
        return d + "Z" if tag == "polygon" else d

    if tag in ("text", "image", "use"):
        warn(f"<{tag}> cannot be expressed as a VectorDrawable path — skipped")
    return None


# --- conversion -------------------------------------------------------------

INHERITED = (
    "fill", "stroke", "stroke-width", "stroke-linecap", "stroke-linejoin",
    "fill-rule", "fill-opacity", "stroke-opacity", "opacity", "stroke-miterlimit",
)


def parse_style(at: dict) -> dict:
    """Fold a style="..." attribute into plain attributes (attributes win)."""
    style = at.get("style")
    if not style:
        return at
    merged = {}
    for decl in style.split(";"):
        if ":" in decl:
            k, v = decl.split(":", 1)
            merged[k.strip()] = v.strip()
    merged.update({k: v for k, v in at.items() if k != "style"})
    return merged


def norm_color(value: str, keep: bool, warn) -> str | None:
    """SVG paint -> #rrggbb, or None for 'no paint'."""
    if value is None:
        return None
    v = value.strip().lower()
    if v in ("none", "transparent"):
        return None
    if v.startswith("url("):
        warn("gradient/pattern paint is not supported — using a flat color")
        return HOUSE_COLOR
    if not keep:
        return HOUSE_COLOR
    if v == "currentcolor":
        return HOUSE_COLOR
    if v.startswith("#"):
        h = v[1:]
        if len(h) == 3:
            h = "".join(ch * 2 for ch in h)
        if len(h) in (6, 8):
            return "#" + h
    m = re.match(r"rgba?\(([^)]*)\)", v)
    if m:
        parts = [p.strip() for p in m.group(1).replace("/", " ").split(",")]
        try:
            r, g, b = (int(round(float(p[:-1]) * 2.55)) if p.endswith("%") else int(float(p))
                       for p in parts[:3])
            return "#%02x%02x%02x" % (r & 255, g & 255, b & 255)
        except ValueError:
            pass
    return HOUSE_COLOR


def local(tag: str) -> str:
    return tag.split("}", 1)[-1]


def convert(svg_text: str, keep_colors: bool, warn) -> str:
    try:
        root = ET.fromstring(svg_text.strip())
    except ET.ParseError as e:
        raise SystemExit(f"not valid SVG/XML: {e}")
    if local(root.tag) != "svg":
        raise SystemExit(f"root element is <{local(root.tag)}>, expected <svg>")

    # Viewport: prefer viewBox, fall back to width/height, default to 24.
    vb = (root.get("viewBox") or "").replace(",", " ").split()
    if len(vb) == 4:
        min_x, min_y, vw, vh = (float(v) for v in vb)
    else:
        min_x = min_y = 0.0
        vw = float(re.sub(r"[^\d.]", "", root.get("width") or "24") or 24)
        vh = float(re.sub(r"[^\d.]", "", root.get("height") or "24") or 24)

    # A non-zero viewBox origin is folded into the root transform so the emitted
    # viewport can always start at 0,0 the way every existing icon does.
    base = (1, 0, 0, 1, -min_x, -min_y)

    paths: list[dict] = []
    seen_unsupported = set()

    def walk(node, inherited: dict, matrix: tuple):
        for child in node:
            tag = local(child.tag)
            if tag in ("defs", "title", "desc", "metadata", "style"):
                continue
            at = parse_style(dict(child.attrib))
            m = matrix
            if at.get("transform"):
                m = mat_mul(matrix, parse_transform(at["transform"]))

            if tag in ("mask", "clipPath", "filter"):
                if tag not in seen_unsupported:
                    seen_unsupported.add(tag)
                    warn(f"<{tag}> ignored — check the result")
                continue

            style = {**inherited, **{k: v for k, v in at.items() if k in INHERITED}}

            if tag in ("g", "svg", "a"):
                walk(child, style, m)
                continue

            d = shape_to_path(tag, at, warn)
            if not d:
                continue
            if at.get("stroke-dasharray"):
                warn("stroke-dasharray is not supported — stroke drawn solid")

            fill = norm_color(style.get("fill", "#000000"), keep_colors, warn)
            stroke = norm_color(style.get("stroke"), keep_colors, warn)
            if tag == "line":
                fill = None  # zero area; SVG's default black fill would be noise
            if fill is None and stroke is None:
                continue

            # SVG scales stroke width by the transform; VectorDrawable has no such
            # notion, so bake the (average) scale in.
            sw = float(style.get("stroke-width", 1) or 1)
            sw *= (math.hypot(m[0], m[1]) + math.hypot(m[2], m[3])) / 2

            opacity = float(style.get("opacity", 1) or 1)
            paths.append({
                "d": transform_path(d, m, warn) if m != IDENTITY else d.strip(),
                "fill": fill,
                "stroke": stroke,
                "stroke_width": sw,
                "cap": style.get("stroke-linecap"),
                "join": style.get("stroke-linejoin"),
                "miter": style.get("stroke-miterlimit"),
                "even_odd": style.get("fill-rule", "").strip() == "evenodd",
                "fill_alpha": float(style.get("fill-opacity", 1) or 1) * opacity,
                "stroke_alpha": float(style.get("stroke-opacity", 1) or 1) * opacity,
            })

        return

    # Seed inheritance from the root's own presentation attributes — Figma puts
    # fill="none" there, and losing it turns every stroke icon into a solid blob.
    root_style = parse_style(dict(root.attrib))
    if root_style.get("transform"):
        base = mat_mul(base, parse_transform(root_style["transform"]))
    walk(root, {k: v for k, v in root_style.items() if k in INHERITED}, base)
    if not paths:
        raise SystemExit("no drawable shapes found in that SVG")

    out = [
        '<vector xmlns:android="http://schemas.android.com/apk/res/android"',
        f'    android:width="{fmt(vw)}dp"',
        f'    android:height="{fmt(vh)}dp"',
        f'    android:viewportWidth="{fmt(vw)}"',
        f'    android:viewportHeight="{fmt(vh)}">',
    ]
    for p in paths:
        attrs = [f'android:pathData="{p["d"]}"']
        if p["fill"]:
            attrs.append(f'android:fillColor="{p["fill"]}"')
            if p["fill_alpha"] < 1:
                attrs.append(f'android:fillAlpha="{fmt(p["fill_alpha"])}"')
            if p["even_odd"]:
                attrs.append('android:fillType="evenOdd"')
        if p["stroke"]:
            attrs.append(f'android:strokeColor="{p["stroke"]}"')
            attrs.append(f'android:strokeWidth="{fmt(p["stroke_width"])}"')
            if p["stroke_alpha"] < 1:
                attrs.append(f'android:strokeAlpha="{fmt(p["stroke_alpha"])}"')
            if p["cap"] in ("round", "square", "butt"):
                attrs.append(f'android:strokeLineCap="{p["cap"]}"')
            if p["join"] in ("round", "bevel", "miter"):
                attrs.append(f'android:strokeLineJoin="{p["join"]}"')
            if p["miter"]:
                attrs.append(f'android:strokeMiterLimit="{fmt(float(p["miter"]))}"')
        out.append("  <path")
        out.extend(f"      {a}" for a in attrs[:-1])
        out.append(f"      {attrs[-1]}/>")
    out.append("</vector>")
    return "\n".join(out) + "\n"


# --- cli --------------------------------------------------------------------


def slug(name: str, prefix: str) -> str:
    """Sanitise to a legal resource name, adding [prefix] only if asked for one."""
    s = re.sub(r"[^a-z0-9_]+", "_", name.strip().lower()).strip("_")
    s = re.sub(r"_+", "_", s)
    if not s:
        raise SystemExit("that name is empty once sanitised")
    if prefix and not s.startswith(prefix):
        s = prefix + s
    if not s[0].isalpha():
        # Better to stop than to invent a family prefix the caller didn't pick.
        raise SystemExit(f"'{s}' must start with a letter — rename it or pass --prefix")
    return s


def read_clipboard() -> str:
    if sys.platform != "darwin":
        raise SystemExit("clipboard input is macOS-only; pass --from instead")
    text = subprocess.run(["pbpaste"], capture_output=True, text=True).stdout
    if "<svg" not in text:
        raise SystemExit(
            "clipboard has no SVG.\n"
            "In Figma: select the icon, then right-click > Copy/Paste as > Copy as SVG."
        )
    return text


def main() -> int:
    root = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
    default_out = os.path.join(root, "android/app/src/main/res/drawable")

    ap = argparse.ArgumentParser(
        description="Convert SVG (clipboard or files) to Android VectorDrawable XML.",
        epilog=(
            "examples:\n"
            "  svg2vd.py oi_mute                    clipboard -> drawable/oi_mute.xml\n"
            "  svg2vd.py i_mic                      same, filled family\n"
            "  svg2vd.py --from ~/icons             every .svg in a folder, names as-is\n"
            "  svg2vd.py --from ~/icons --prefix oi_   ...with one family prefix\n"
            "  svg2vd.py --from pin.svg i_pin       one file\n"
            "  svg2vd.py --stdout oi_mute           print instead of writing\n"
        ),
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    ap.add_argument("name", nargs="?", help="resource name, used as given")
    ap.add_argument("--from", dest="src", help="an .svg file or a folder of them")
    ap.add_argument("--out", default=default_out, help="drawable dir (default: %(default)s)")
    ap.add_argument("--prefix", default="",
                    help="prepend to every name unless already present, e.g. oi_")
    ap.add_argument("--keep-colors", action="store_true",
                    help=f"keep source colors instead of normalising to {HOUSE_COLOR}")
    ap.add_argument("--force", action="store_true", help="overwrite existing files")
    ap.add_argument("--stdout", action="store_true", help="print XML, write nothing")
    args = ap.parse_args()

    jobs: list[tuple[str, str]] = []  # (name, svg text)
    if args.src:
        if os.path.isdir(args.src):
            files = sorted(f for f in os.listdir(args.src) if f.lower().endswith(".svg"))
            if not files:
                raise SystemExit(f"no .svg files in {args.src}")
            for f in files:
                with open(os.path.join(args.src, f), encoding="utf-8") as fh:
                    jobs.append((os.path.splitext(f)[0], fh.read()))
        else:
            with open(args.src, encoding="utf-8") as fh:
                jobs.append((args.name or os.path.splitext(os.path.basename(args.src))[0],
                             fh.read()))
    else:
        if not args.name:
            ap.error("a name is required when reading the clipboard")
        jobs.append((args.name, read_clipboard()))

    failures = 0
    for raw_name, text in jobs:
        name = slug(raw_name, args.prefix)
        warnings: list[str] = []
        try:
            xml = convert(text, args.keep_colors, warnings.append)
        except SystemExit as e:
            print(f"  {name}: {e}", file=sys.stderr)
            failures += 1
            continue

        for w in dict.fromkeys(warnings):
            print(f"  {name}: warning: {w}", file=sys.stderr)

        if args.stdout:
            print(xml, end="")
            continue

        dest = os.path.join(args.out, name + ".xml")
        if os.path.exists(dest) and not args.force:
            print(f"  {name}: exists already — pass --force to overwrite", file=sys.stderr)
            failures += 1
            continue
        os.makedirs(args.out, exist_ok=True)
        with open(dest, "w", encoding="utf-8") as fh:
            fh.write(xml)
        print(f"  {os.path.relpath(dest, root)}   R.drawable.{name}")

    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
