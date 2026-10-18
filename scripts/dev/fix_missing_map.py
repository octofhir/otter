#!/usr/bin/env python3
"""Insert `map: TypeMap::NEW` into struct literals cargo reports as missing it.

usage: cargo check ... 2>&1 | fix_missing_map.py
Reads `missing field `map` in initializer of `T`` errors with their
`--> file:line:col` locations and inserts the field right after the
literal's opening brace.
"""
import collections
import re
import sys

text = sys.stdin.read()
sites = collections.defaultdict(list)
for m in re.finditer(r"missing field `map` in initializer of `[^`]+`\s*\n\s*--> ([^:]+):(\d+):(\d+)", text):
    sites[m.group(1)].append((int(m.group(2)), int(m.group(3))))
for path, locs in sites.items():
    lines = open(path).read().split("\n")
    for line, col in sorted(locs, reverse=True):
        row = line - 1
        idx = lines[row].find("{", col - 1)
        while idx < 0:
            row += 1
            idx = lines[row].find("{")
        rest = lines[row][idx + 1:]
        indent = re.match(r"\s*", lines[row]).group(0) + "    "
        if rest.strip() and not rest.strip().startswith("//"):
            lines[row] = lines[row][:idx + 1] + " map: otter_gc::cell_map::TypeMap::NEW," + rest
        else:
            lines.insert(row + 1, indent + "map: otter_gc::cell_map::TypeMap::NEW,")
    open(path, "w").write("\n".join(lines))
    print(path, len(locs))
