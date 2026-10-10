#!/usr/bin/env python3
"""Aggregate self samples of optimized code by graph IR node kind.

usage: node_kinds.py <artifact-dir> <offsets.txt> [top]

<artifact-dir> is an `otter --jit-artifacts=<dir>` dump and <offsets.txt> the
`== self` rows of a `launchprof.py` run with OFFSETS=1 on the same program
(code object ids are deterministic across runs). Each sample
`JS:<name> [optimizing c<id>]+0x<off>` is credited to the node region of
code object <id> holding the offset; samples outside every node region count
as `prologue` (before the first region) or by their region kind otherwise.
Also prints the share of samples whose code object has no artifact.
"""
import collections
import pathlib
import re
import sys

root = pathlib.Path(sys.argv[1])
top = int(sys.argv[3]) if len(sys.argv) > 3 else 40
dirs = {}
for entry in root.iterdir():
    m = re.match(r"jit-\d+-(\w+)-f\d+-c(\d+)$", entry.name)
    if m:
        dirs[int(m.group(2))] = entry

regions_cache = {}


def regions(code_id):
    if code_id not in regions_cache:
        out = []
        path = dirs.get(code_id)
        if path is not None:
            for line in open(path / "asm.txt"):
                m = re.match(r"\s*; region kind=(\S+) range=\+0x([0-9a-f]+)\.\.\+0x([0-9a-f]+)(.*)", line)
                if m:
                    op = re.search(r'tier-op="v\d+ ([A-Za-z0-9]+)', m.group(4))
                    out.append((int(m.group(2), 16), int(m.group(3), 16), op.group(1) if op else m.group(1)))
        regions_cache[code_id] = out
    return regions_cache[code_id]


kinds = collections.Counter()
tiers = collections.Counter()
missing = 0
total = 0
for line in open(sys.argv[2]):
    m = re.match(r"\s*(\d+)\s+JS:.*\[(\w+) c(\d+)\]\+0x([0-9a-f]+)$", line.strip())
    if not m:
        continue
    count, tier, code_id, offset = int(m.group(1)), m.group(2), int(m.group(3)), int(m.group(4), 16) & ~3
    total += count
    tiers[tier] += count
    if tier != "optimizing":
        continue
    rs = regions(code_id)
    if not rs:
        missing += count
        continue
    name = "prologue" if offset < rs[0][0] else "unmapped"
    for start, end, kind in rs:
        if start <= offset < end:
            name = kind
            break
    kinds[name] += count
print(f"generated samples={total} tiers={dict(tiers)} optimizing-without-artifact={missing}")
opt = sum(kinds.values())
for name, count in kinds.most_common(top):
    print(f"{count:6} {100 * count / max(opt, 1):5.1f}% {name}")
