#!/usr/bin/env python3
"""Self-sample profile of the `otter-isolate` thread from a `sample` report.

usage: selfprof.py <sample.txt> [top]
JIT frames (unknown binary) fold into one `JIT` row.
"""
import collections, re, sys

text = open(sys.argv[1]).read()
top = int(sys.argv[2]) if len(sys.argv) > 2 else 30
graph = text.split("Call graph:", 1)[1].split("Total number in stack", 1)[0]
thread = graph.split("otter-isolate", 1)[1]
thread = re.split(r"\n    \d+ Thread_", thread, maxsplit=1)[0]
entries = []
for line in thread.splitlines()[1:]:
    m = re.match(r"^([\s+!:|]*?)(\d+) (.*)$", line)
    if m:
        entries.append((len(m.group(1)), int(m.group(2)), m.group(3)))
counts = collections.Counter()
total = 0
for i, (depth, count, name) in enumerate(entries):
    children, j = 0, i + 1
    while j < len(entries) and entries[j][0] > depth:
        if entries[j][0] == entries[i + 1][0]:
            children += entries[j][1]
        j += 1
    own = count - children
    if own <= 0:
        continue
    total += own
    counts["JIT" if name.startswith("???") else re.sub(r"\s*\(in .*", "", name)[:130]] += own
print(f"isolate self samples: {total}")
for name, count in counts.most_common(top):
    print(f"{count:7d}  {name}")
