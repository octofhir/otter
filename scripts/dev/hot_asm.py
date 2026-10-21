#!/usr/bin/env python3
"""Print a code object's disassembly between two offsets with self samples.
usage: hot_asm.py <artifact-dir> <self-offsets.txt> <name-substring> <start> <end>
Offsets are hex or decimal; samples come from launchprof OFFSETS=1 self rows.
Inclusive rows do not contribute to the per-instruction sample counts.
"""
import re
import sys

art, offsets, name, start, end = sys.argv[1:6]
start, end = int(start, 0), int(end, 0)
hot = {}
self_samples = open(offsets).read().split("== self", 1)[-1].split("== inclusive", 1)[0]
for line in self_samples.splitlines():
    m = re.match(r"\s*(\d+)\s+JS:(.*)\+0x([0-9a-f]+)$", line.rstrip())
    if m and name in m.group(2):
        # Sampled pcs are not always instruction-aligned; credit the
        # instruction holding them.
        offset = int(m.group(3), 16) & ~3
        hot[offset] = hot.get(offset, 0) + int(m.group(1))
for line in open(art + "/asm.txt"):
    m = re.match(r"\+0x([0-9a-f]+):", line)
    if not m:
        if line.startswith("L") and start <= int(line[1:9], 16) < end:
            print("     " + line.rstrip())
        continue
    off = int(m.group(1), 16)
    if start <= off < end:
        print(f"{hot.get(off, ''):>4} {line.rstrip()}")
