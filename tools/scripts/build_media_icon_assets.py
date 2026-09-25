#!/usr/bin/env python3
"""Rebuild the media/camera icon assets using the project's SVG converter."""
from pathlib import Path
import argparse
import importlib.util
import json
import re
import shutil
import sys
import xml.etree.ElementTree as ET

sys.dont_write_bytecode = True
ANDROID = '{http://schemas.android.com/apk/res/android}'

def build(sources, catalog, resources, converter):
    spec = importlib.util.spec_from_file_location('svg2vd', converter)
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    if catalog is not None:
        entries = json.loads(catalog.read_text())
        files = [(sources / entry['kind'] / (entry['name'] + '.svg'), entry['kind'] == 'controls') for entry in entries if not entry.get('reused')]
    else:
        # SVGs are the source of truth; morph endpoint references are excluded.
        files = [(p, False) for p in sorted((sources / 'outlined').glob('*.svg'))]
        files += [(p, False) for p in sorted((sources / 'filled').glob('*.svg'))]
        files += [(p, True) for p in sorted((sources / 'controls').glob('*.svg'))]
    files += [(p, True) for p in sorted((sources / 'controls' / 'file-parts').glob('*.svg'))]
    (resources / 'drawable').mkdir(parents=True, exist_ok=True)
    (resources / 'raw').mkdir(parents=True, exist_ok=True)
    for src, keep_colors in files:
        source = src.read_text()
        warnings = []
        xml = mod.convert(source, keep_colors, warnings.append)
        if warnings:
            raise RuntimeError(f'{src.name}: {warnings}')
        root = ET.fromstring(source)
        # svg2vd derives intrinsic size from viewBox. Controls intentionally have
        # intrinsic dp sizes different from their editable 24-unit geometry.
        for dimension in ('width', 'height'):
            size = root.get(dimension)
            if size and re.fullmatch(r'\d+(?:\.\d+)?', size):
                xml = re.sub(rf'android:{dimension}="[^"]+"', f'android:{dimension}="{size}dp"', xml, count=1)
        ET.fromstring(xml)
        (resources / 'drawable' / (src.stem + '.xml')).write_text(xml)
    animations = sorted((sources / 'lottie').glob('*.json'))
    for source in animations:
        json.loads(source.read_text())
        shutil.copyfile(source, resources / 'raw' / source.name)
    return len(files), len(animations)

if __name__ == '__main__':
    repo = Path(__file__).resolve().parents[2]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--sources', type=Path, default=repo / 'design/icons')
    parser.add_argument('--catalog', type=Path, help='Optional legacy catalog; otherwise discover SVG sources directly')
    parser.add_argument('--resources', type=Path, default=repo / 'android/app/src/main/res')
    parser.add_argument('--converter', type=Path, default=repo / 'tools/scripts/svg2vd.py')
    args = parser.parse_args()
    count, motion = build(args.sources, args.catalog, args.resources, args.converter)
    print(f'Generated {count} drawables and copied {motion} Lottie assets.')
