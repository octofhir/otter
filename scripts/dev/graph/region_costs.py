#!/usr/bin/env python3
"""Per-node static instruction count and sample count of a hotasm listing.

usage: region_costs.py <hot-N.txt> [min-samples]
Prints every code-map region with its sample total, its instruction count and
its node, in code order, for regions with at least `min-samples` samples.
"""
import re
import sys

path = sys.argv[1]
floor = int(sys.argv[2]) if len(sys.argv) > 2 else 1
region = None
rows = []
for line in open(path):
    m = re.match(r'\s*\d+\s+; region kind=(\S+) .*?(?:tier-op="([^"]*)")?\s*$', line)
    if m:
        region = [m.group(2) or m.group(1), 0, 0]
        rows.append(region)
        continue
    m = re.match(r'\s*(\d+)\s+\+0x[0-9a-f]+:', line)
    if m and region is not None:
        region[1] += 1
        region[2] += int(m.group(1))
for name, count, samples in rows:
    if samples >= floor:
        print(f"{samples:6d} {count:5d}  {name[:120]}")
