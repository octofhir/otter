#!/usr/bin/env python3
"""Isolate-thread self samples split by execution tier, from macOS `sample`.

usage: tier_split.py <sample.txt> <perf-map> [top]
Generated-code leaves are named through the run's `--perf-basic-prof` map
and summed by tier (`optimizing`, `template`, ...); native leaves are summed
by crate (`otter_vm`, `otter_gc`, ...) and listed by symbol.
"""
import bisect, collections, re, sys

sample, perf_map = sys.argv[1], sys.argv[2]
top = int(sys.argv[3]) if len(sys.argv) > 3 else 25
maps = []
for line in open(perf_map):
    address, size, name = line.split(" ", 2)
    maps.append((int(address, 16), int(size, 16), name.strip()))
maps.sort()
starts = [m[0] for m in maps]
text = open(sample).read()
graph = text.split("Call graph:", 1)[1].split("Total number in stack", 1)[0]
thread = next(t for t in re.split(r"\n    (?=\d+ Thread_)", graph) if "otter-isolate" in t.split("\n", 1)[0])
rows = []
for line in thread.splitlines():
    m = re.match(r"^([\s+!:|]*)(\d+) (.*)$", line)
    if m:
        rows.append((len(m.group(1)), int(m.group(2)), m.group(3)))
tiers = collections.Counter()
symbols = collections.Counter()
total = 0
for i, (depth, count, frame) in enumerate(rows):
    j, kids, first = i + 1, 0, None
    while j < len(rows) and rows[j][0] > depth:
        if first is None:
            first = rows[j][0]
        if rows[j][0] == first:
            kids += rows[j][1]
        j += 1
    own = count - kids
    if own <= 0:
        continue
    total += own
    if frame.startswith("???"):
        addresses = [int(a, 16) for a in re.findall(r"0x([0-9a-f]+)", frame.split("[", 1)[1])]
        for address in addresses:
            k = bisect.bisect_right(starts, address) - 1
            share = own / len(addresses)
            if k >= 0 and address < maps[k][0] + maps[k][1]:
                m = re.search(r"\[(\w+) c\d+\]", maps[k][2])
                tiers["jit:" + (m.group(1) if m else "?")] += share
            else:
                tiers["jit:unmapped"] += share
        continue
    name = re.sub(r"\s*\(in [^)]*\).*", "", frame).strip()
    m = re.search(r"(otter_[a-z0-9_]+)", name)
    crate = m.group(1) if m else ("libsystem" if "_platform" in name or "psynch" in name else "other")
    tiers[crate] += own
    symbols[name[:110]] += own
print(f"isolate self samples {total}")
for name, value in tiers.most_common():
    print(f"{100 * value / total:5.1f}% {name}")
print("-- native symbols")
for name, value in symbols.most_common(top):
    print(f"{value:6.0f} {name}")
