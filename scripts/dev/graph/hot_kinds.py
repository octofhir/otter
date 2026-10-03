#!/usr/bin/env python3
"""Aggregate a hotasm.py profile of graph-tier code by node kind.

usage: hot_kinds.py <hotasm-out-dir> [top]
Reads every hot-<n>.txt that hotasm.py wrote, attributes each instruction's
samples to the graph node region it lies in (generic nodes by bytecode
opcode), and prints the kinds with the most samples plus the hottest
individual generic sites.
"""
import collections, pathlib, re, sys

out = pathlib.Path(sys.argv[1])
top = int(sys.argv[2]) if len(sys.argv) > 2 else 30
kinds = collections.Counter()
sites = collections.Counter()
for path in sorted(out.glob("hot-*.txt")):
    region = None
    for line in path.read_text().splitlines():
        m = re.search(r'function=(\d+) pc=(\d+).*tier-op="v\d+ ([A-Za-z0-9]+)', line)
        if m:
            region = (m.group(1), m.group(2), m.group(3))
            continue
        if "region kind=out-of-line" in line:
            region = ("-", "-", "out-of-line")
            continue
        s = re.match(r"\s*(\d+)\s+\+0x", line)
        if s and region:
            n = int(s.group(1))
            kinds[region[2]] += n
            if region[2] == "Generic":
                sites[(region[0], region[1])] += n
total = sum(kinds.values())
print(f"attributed samples: {total}")
for kind, n in kinds.most_common(top):
    print(f"  {n:6d}  {kind}")
print("hottest generic sites (function, pc):")
for site, n in sites.most_common(15):
    print(f"  {n:6d}  {site}")
