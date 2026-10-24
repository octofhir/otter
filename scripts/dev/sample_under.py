#!/usr/bin/env python3
"""Self samples of the frames below every frame matching a pattern on the
otter-isolate thread of one macOS `sample` report, plus the direct callees.

usage: sample_under.py <sample.txt> <regex> [top]
"""
import collections, re, sys

text = open(sys.argv[1]).read()
rx = re.compile(sys.argv[2])
top = int(sys.argv[3]) if len(sys.argv) > 3 else 25
graph = text.split("Call graph:", 1)[1].split("Total number in stack", 1)[0]
thread = graph.split("otter-isolate", 1)[1]
thread = re.split(r"\n    \d+ Thread_", thread, maxsplit=1)[0]
entries = []
for line in thread.splitlines()[1:]:
    m = re.match(r"^([\s+!:|]*?)(\d+) (.*)$", line)
    if m:
        entries.append((len(m.group(1)), int(m.group(2)), m.group(3)))


def short(name):
    name = re.sub(r"\s+\(in .*$", "", name)
    name = re.sub(r"\s+\+ \d+.*$", "", name)
    parts = re.findall(r"\d+([A-Za-z_][A-Za-z0-9_]*)", name)
    return "::".join(parts[-2:]) if parts else name[:90]


own = collections.Counter()
direct = collections.Counter()
total = 0
inside = []
for i, (depth, count, name) in enumerate(entries):
    while inside and inside[-1] >= depth:
        inside.pop()
    children = 0
    j = i + 1
    while j < len(entries) and entries[j][0] > depth:
        if entries[j][0] == entries[i + 1][0]:
            children += entries[j][1]
        j += 1
    if inside:
        if depth == inside[-1] + 2 or (i > 0 and entries[i - 1][0] == inside[-1]):
            pass
        if count - children > 0:
            own[short(name)] += count - children
    if rx.search(name) and not inside:
        total += count
        inside.append(depth)
        j = i + 1
        while j < len(entries) and entries[j][0] > depth:
            if entries[j][0] == entries[i + 1][0]:
                direct[short(entries[j][2])] += entries[j][1]
            j += 1
        if count - children > 0:
            own[short(name)] += count - children
print(f"inclusive {total}")
print("-- direct callees")
for k, v in direct.most_common(12):
    print(f"  {v:5d} {k}")
print("-- self below")
for k, v in own.most_common(top):
    print(f"  {v:5d} {k}")
