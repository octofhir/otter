#!/usr/bin/env python3
"""Inclusive samples of each direct child of a frame in a macOS `sample` report.

usage: childprof.py <sample.txt> <frame-substring> [<frame-substring>...]
For every frame whose name contains a substring, sums the inclusive sample
counts of its direct callees, so a compile pipeline splits into phases.
"""
import collections, re, sys

text = open(sys.argv[1]).read()
graph = text.split("Call graph:", 1)[1].split("Total number in stack", 1)[0]
entries = []
for line in graph.splitlines():
    m = re.match(r"^([\s+!:|]*?)(\d+) (.*)$", line)
    if m:
        entries.append((len(m.group(1)), int(m.group(2)), m.group(3)))
for needle in sys.argv[2:]:
    children = collections.Counter()
    total = 0
    for i, (depth, count, name) in enumerate(entries):
        if needle not in name:
            continue
        total += count
        j, kid = i + 1, None
        while j < len(entries) and entries[j][0] > depth:
            kid = entries[j][0] if kid is None else kid
            if entries[j][0] == kid:
                children[re.sub(r"\s*\(in .*", "", entries[j][2])[-95:]] += entries[j][1]
            j += 1
    print(f"== {needle} {total}")
    for name, count in children.most_common(14):
        print(f"{count:7d}  {name}")
