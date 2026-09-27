#!/usr/bin/env python3
"""Self-sample profile of one call subtree from a macOS `sample` report.

usage: subtreeprof.py <sample.txt> <frame-substring> [top]
Sums, per function name, the self samples of every frame nested under any
frame whose name contains <frame-substring> (e.g. `compile_optimized_function`).
"""
import collections, re, sys

text = open(sys.argv[1]).read()
needle = sys.argv[2]
top = int(sys.argv[3]) if len(sys.argv) > 3 else 30
graph = text.split("Call graph:", 1)[1].split("Total number in stack", 1)[0]
entries = []
for line in graph.splitlines():
    m = re.match(r"^([\s+!:|]*?)(\d+) (.*)$", line)
    if m:
        entries.append((len(m.group(1)), int(m.group(2)), m.group(3)))
selfc = collections.Counter()
total = 0
inside = []  # depths of enclosing matching frames
for i, (depth, count, name) in enumerate(entries):
    while inside and depth <= inside[-1]:
        inside.pop()
    matched = needle in name
    if matched and not inside:
        total += count
    if matched:
        inside.append(depth)
    if not inside:
        continue
    children, j = 0, i + 1
    while j < len(entries) and entries[j][0] > depth:
        if entries[j][0] == entries[i + 1][0]:
            children += entries[j][1]
        j += 1
    own = count - children
    if own > 0:
        selfc[re.sub(r"\s*\(in .*", "", name)[:110]] += own
print(f"subtree samples: {total}")
for name, count in selfc.most_common(top):
    print(f"{count:7d}  {name}")
