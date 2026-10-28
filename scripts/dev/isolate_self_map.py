#!/usr/bin/env python3
"""Self samples of the otter-isolate thread, generated code split by the
perf map: each routine by name, optimized and template code by tier.

usage: isolate_self_map.py <sample.txt> <perf.map> [top]
"""
import bisect, collections, re, sys

text = open(sys.argv[1]).read()
regions = []
for line in open(sys.argv[2]):
    address, size, name = line.rstrip("\n").split(" ", 2)
    regions.append((int(address, 16), int(size, 16), name))
regions.sort()
starts = [region[0] for region in regions]
top = int(sys.argv[3]) if len(sys.argv) > 3 else 30
graph = text.split("Call graph:", 1)[1].split("Total number in stack", 1)[0]
thread = graph.split("otter-isolate", 1)[1].split("\n    ", 1)[1]
thread = re.split(r"\n    \d+ Thread_", thread, maxsplit=1)[0]
entries = []
for line in thread.splitlines():
    m = re.match(r"^([\s+!:|]*?)(\d+) (.*)$", line)
    if m:
        entries.append((len(m.group(1)), int(m.group(2)), m.group(3)))


def generated(name):
    m = re.search(r"\[0x([0-9a-f]+)\]", name)
    if not m:
        return "(generated code)"
    address = int(m.group(1), 16)
    i = bisect.bisect_right(starts, address) - 1
    if i < 0 or address >= regions[i][0] + regions[i][1]:
        return "(generated code, unmapped)"
    label = regions[i][2]
    if label.startswith("routine"):
        return label
    for tier in ("optimizing", "template"):
        if tier in label:
            return f"(generated {tier})"
    return "(generated other)"


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
        own_counts[generated(name)] += own
    else:
        own_counts[re.sub(r"\s*\(in .*", "", name)[:110]] += own
print(f"isolate self samples: {total}")
for name, count in own_counts.most_common(top):
    print(f"{count:7d}  {100.0 * count / max(total, 1):5.1f}%  {name}")
