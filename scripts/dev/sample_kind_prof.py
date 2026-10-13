#!/usr/bin/env python3
"""Self samples of optimized code per graph node kind, from macOS `sample`.

usage: sample_kind_prof.py <otter-binary> <fixture.js> <out-dir> [seconds] [top]

Runs the fixture once with `--perf-basic-prof --jit-artifacts=<out-dir>/art`,
samples the isolate thread at 1 ms, then credits every self sample in a
generated-code frame to the code-map region holding its address in that same
run's artifact bundle, summed by the region's node kind (its `Kind` name
without the payload). Prints the share of all isolate samples per kind, the
share of generated code overall, and the hottest optimized functions.
The `sample_tree.py`-style counterpart of `node_kind_prof.py`, which reads
xctrace offsets instead.
"""
import bisect
import collections
import glob
import os
import re
import subprocess
import sys
import time

binary, fixture, out = sys.argv[1], sys.argv[2], sys.argv[3]
seconds = sys.argv[4] if len(sys.argv) > 4 else "30"
top = int(sys.argv[5]) if len(sys.argv) > 5 else 25
os.makedirs(out, exist_ok=True)
art = os.path.join(out, "art")
subprocess.run(["rm", "-rf", art], check=True)
proc = subprocess.Popen(
    [binary, "--perf-basic-prof", f"--jit-artifacts={art}", fixture], stdout=subprocess.DEVNULL
)
time.sleep(0.2)
report = os.path.join(out, "sample.txt")
subprocess.run(["sample", str(proc.pid), seconds, "1", "-file", report],
               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
proc.wait()
perf_map = f"/tmp/perf-{proc.pid}.map"

maps = []
for line in open(perf_map):
    address, size, name = line.split(" ", 2)
    maps.append((int(address, 16), int(size, 16), name.strip()))
maps.sort()
starts = [m[0] for m in maps]

regions = {}


def regions_of(code):
    if code not in regions:
        found = []
        for path in glob.glob(os.path.join(art, f"*-c{code}")):
            for line in open(os.path.join(path, "asm.txt")):
                m = re.search(r'range=\+0x([0-9a-f]+)\.\.\+0x([0-9a-f]+).*?tier-op="([^"]*)"', line)
                if m:
                    kind = re.sub(r"^v\d+ ", "", m.group(3))
                    kind = re.split(r"[ ({]", kind, maxsplit=1)[0]
                    found.append((int(m.group(1), 16), int(m.group(2), 16), kind))
        regions[code] = sorted(found)
    return regions[code]


text = open(report).read()
graph = text.split("Call graph:", 1)[1].split("Total number in stack", 1)[0]
thread = next(t for t in re.split(r"\n    (?=\d+ Thread_)", graph) if "otter-isolate" in t.split("\n", 1)[0])
rows = []
for line in thread.splitlines():
    m = re.match(r"^([\s+!:|]*)(\d+) (.*)$", line)
    if m:
        rows.append((len(m.group(1)), int(m.group(2)), m.group(3)))
total = rows[0][1]
kinds = collections.Counter()
functions = collections.Counter()
jit = 0.0
for i, (depth, count, frame) in enumerate(rows):
    children, j, first = 0, i + 1, None
    while j < len(rows) and rows[j][0] > depth:
        if first is None:
            first = rows[j][0]
        if rows[j][0] == first:
            children += rows[j][1]
        j += 1
    own = count - children
    if own <= 0 or "???" not in frame or "[" not in frame:
        continue
    addresses = [int(a, 16) for a in re.findall(r"0x([0-9a-f]+)", frame.split("[", 1)[1])]
    share = own / len(addresses)
    for address in addresses:
        k = bisect.bisect_right(starts, address) - 1
        if k < 0 or address >= maps[k][0] + maps[k][1]:
            continue
        name = maps[k][2]
        jit += share
        functions[name] += share
        m = re.search(r"\[(\w+) c(\d+)\]", name)
        if not m or m.group(1) != "optimizing":
            kinds["<" + (m.group(1) if m else "?") + ">"] += share
            continue
        offset = address - maps[k][0]
        hit = [r for r in regions_of(m.group(2)) if r[0] <= offset < r[1]]
        kinds[hit[-1][2] if hit else "<outside>"] += share
print(f"isolate samples {total}, generated code self {100 * jit / total:.1f}%")
for kind, value in kinds.most_common(top):
    print(f"{100 * value / total:5.1f}% {kind}")
print("-- functions")
for name, value in functions.most_common(10):
    print(f"{100 * value / total:5.1f}% {name}")
