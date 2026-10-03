#!/usr/bin/env python3
"""Self samples of the otter-isolate thread in one `sample` report.

usage: isolate_self.py <sample.txt> [top]
Prints the leaf functions by self samples, generated code (unsymbolized
frames) counted as one row, and the inclusive share of a few runtime entry
families.
"""
import collections, re, sys

text = open(sys.argv[1]).read()
top = int(sys.argv[2]) if len(sys.argv) > 2 else 30
graph = text.split("Call graph:", 1)[1].split("Total number in stack", 1)[0]
thread = graph.split("otter-isolate", 1)[1].split("\n    ", 1)[1]
thread = re.split(r"\n    \d+ Thread_", thread, maxsplit=1)[0]
entries = []
for line in thread.splitlines():
    m = re.match(r"^([\s+!:|]*?)(\d+) (.*)$", line)
    if m:
        entries.append((len(m.group(1)), int(m.group(2)), m.group(3)))
own_counts = collections.Counter()
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
    if "???" in name:
        own_counts["(generated code)"] += own
    else:
        own_counts[re.sub(r"\s*\(in .*", "", name)[:110]] += own
print(f"isolate self samples: {total}")
for name, count in own_counts.most_common(top):
    print(f"{count:7d}  {100.0 * count / max(total, 1):5.1f}%  {name}")
