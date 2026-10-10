#!/usr/bin/env python3
"""Self samples of optimized code per graph node kind.

usage: node_kind_prof.py <artifact-root> <launchprof-offsets.txt> [top]

<artifact-root> is a `--jit-artifacts` directory from the same binary and
script (code object ids are deterministic per run); the offsets file is a
launchprof `OFFSETS=1 TOP=100000` report. Every `JS:<name> [optimizing cN]+0x..`
self row is credited to the code-map region of code object N holding that
offset, then summed by the region's operation kind (the node's `Kind` name,
without its payload). Samples outside every region (prologue, exits,
veneers, deferred slow paths) are reported as `<outside>`.
"""
import bisect
import collections
import glob
import json
import os
import re
import sys

root, offsets_file = sys.argv[1], sys.argv[2]
top = int(sys.argv[3]) if len(sys.argv) > 3 else 40

maps = {}
for directory in glob.glob(os.path.join(root, "*-optimizing-*")):
    match = re.search(r"-c(\d+)$", directory)
    path = os.path.join(directory, "code-map.json")
    if not match or not os.path.exists(path):
        continue
    regions = []
    for region in json.load(open(path))["regions"]:
        if region.get("kind") != "instruction" or region["endOffset"] <= region["startOffset"]:
            continue
        operation = region["operation"].split(" ", 1)[1] if " " in region["operation"] else region["operation"]
        kind = re.split(r"[ ({\[]", operation, maxsplit=1)[0]
        regions.append((region["startOffset"], region["endOffset"], kind))
    regions.sort()
    maps[int(match.group(1))] = regions

text = open(offsets_file).read()
self_section = text.split("== self", 1)[1].split("== inclusive", 1)[0]
by_kind = collections.Counter()
total = 0
for line in self_section.splitlines():
    match = re.match(r"\s*(\d+)\s+JS:.*\[optimizing c(\d+)\]\+0x([0-9a-f]+)$", line.rstrip())
    if not match:
        continue
    samples, code, offset = int(match.group(1)), int(match.group(2)), int(match.group(3), 16) & ~3
    total += samples
    regions = maps.get(code)
    if regions is None:
        by_kind["<no artifact>"] += samples
        continue
    index = bisect.bisect_right(regions, (offset, float("inf"), "")) - 1
    if index >= 0 and regions[index][0] <= offset < regions[index][1]:
        by_kind[regions[index][2]] += samples
    else:
        by_kind["<outside>"] += samples

print(f"optimized self samples: {total}")
for kind, samples in by_kind.most_common(top):
    print(f"{samples:7d} {100.0 * samples / max(total, 1):5.1f}%  {kind}")
