#!/usr/bin/env python3
"""Preserve Cargo's artifact messages before cargo-ndk consumes them."""

import os
import subprocess
import sys

cargo = os.environ["PROMTUZ_REAL_CARGO"]
environment = dict(os.environ, CARGO=cargo)
with open(os.environ["PROMTUZ_RUST_ARTIFACTS"], "ab") as log:
    with subprocess.Popen([cargo, *sys.argv[1:]], env=environment, stdout=subprocess.PIPE) as child:
        for line in child.stdout:
            log.write(line)
            sys.stdout.buffer.write(line)
            sys.stdout.buffer.flush()
        sys.exit(child.wait())
