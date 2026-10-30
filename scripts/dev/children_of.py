#!/usr/bin/env python3
"""Inclusive samples of the frames directly below every frame matching a
substring, on the otter-isolate thread of a `sample` report: what a runtime
entry (e.g. `otter_native_invoke`) actually spends its time in.

usage: children_of.py <sample.txt> <substring> [top] [depth]
`depth` (default 1) descends that many named levels, skipping unsymbolized
generated-code frames and recursive matches, and reports the frame found there.
"""
import collections, re, sys

text = open(sys.argv[1]).read()
needle = sys.argv[2]
top = int(sys.argv[3]) if len(sys.argv) > 3 else 30
depth_wanted = int(sys.argv[4]) if len(sys.argv) > 4 else 1
graph = text.split("Call graph:", 1)[1].split("Total number in stack", 1)[0]
threads = re.split(r"\n    (?=\d+ Thread_)", graph)
thread = next(t for t in threads if t.strip() and "otter-isolate" in t.strip().splitlines()[0])
entries = []
for line in thread.splitlines():
    m = re.match(r"^([\s+!:|]*?)(\d+) (\S+)", line)
    if m:
        entries.append((len(m.group(1)), int(m.group(2)), m.group(3)))
total = entries[0][1] if entries else 1
counts = collections.Counter()
i = 0
while i < len(entries):
    depth, count, name = entries[i]
    if needle not in name:
        i += 1
        continue
    # Children of this frame: following entries one level deeper, until the
    # subtree ends. Descend `depth_wanted` named levels.
    frontier = [i]
    for _ in range(depth_wanted):
        nxt = []
        for p in frontier:
            pd = entries[p][0]
            j = p + 1
            child_depth = None
            while j < len(entries) and entries[j][0] > pd:
                if child_depth is None:
                    child_depth = entries[j][0]
                if entries[j][0] == child_depth:
                    nxt.append(j)
                j += 1
        # Skip generated frames: descend through them without counting a level.
        resolved = []
        while nxt:
            k = nxt.pop()
            if entries[k][2] == "???" or needle in entries[k][2]:
                kd = entries[k][0]
                j = k + 1
                cd = None
                while j < len(entries) and entries[j][0] > kd:
                    if cd is None:
                        cd = entries[j][0]
                    if entries[j][0] == cd:
                        nxt.append(j)
                    j += 1
            else:
                resolved.append(k)
        frontier = resolved
    for k in frontier:
        counts[entries[k][2]] += entries[k][1]
    # Skip this subtree so nested matches are not double counted.
    j = i + 1
    while j < len(entries) and entries[j][0] > depth:
        j += 1
    i = j
for name, c in counts.most_common(top):
    print(f"{c:6} {100 * c / total:5.1f}%  {name[:140]}")
