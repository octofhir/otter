#!/usr/bin/env python3
"""Samples under frames matching a pattern, by the tier of the nearest
generated-code frame above them, from a macOS `sample` report of the
otter-isolate thread and the run's `--perf-basic-prof` map.

usage: caller_tier.py <sample.txt> <perf.map> <regex> [--name]
`--name` reports the calling code object's full perf-map name (JS function
and tier) instead of its tier alone.
"""
import bisect, collections, re, sys

text, perf, rx = open(sys.argv[1]).read(), sys.argv[2], re.compile(sys.argv[3])
by_name = "--name" in sys.argv[4:]
ranges = []
for line in open(perf):
    parts = line.rstrip('\n').split(' ', 2)
    if len(parts) == 3:
        ranges.append((int(parts[0], 16), int(parts[1], 16), parts[2]))
ranges.sort()
starts = [r[0] for r in ranges]

def tier(addr):
    i = bisect.bisect_right(starts, addr) - 1
    if i >= 0 and ranges[i][0] <= addr < ranges[i][0] + ranges[i][1]:
        if by_name:
            return ranges[i][2]
        m = re.search(r'\[(\w+)', ranges[i][2])
        return m.group(1) if m else ranges[i][2]
    return None

graph = text.split("Call graph:", 1)[1].split("Total number in stack", 1)[0]
thread = graph.split("otter-isolate", 1)[1]
thread = re.split(r"\n    \d+ Thread_", thread, maxsplit=1)[0]
stack, counts = [], collections.Counter()
for line in thread.splitlines()[1:]:
    m = re.match(r"^([\s+!:|]*?)(\d+) (.*)$", line)
    if not m:
        continue
    depth, count, name = len(m.group(1)), int(m.group(2)), m.group(3)
    while stack and stack[-1][0] >= depth:
        stack.pop()
    addr = None
    if name.startswith('???'):
        a = re.search(r'\[0x([0-9a-f]+)\]', name)
        addr = int(a.group(1), 16) if a else None
    stack.append((depth, name, addr))
    if rx.search(name) and not any(rx.search(n) for _, n, _ in stack[:-1]):
        caller = next((tier(a) for _, _, a in reversed(stack[:-1]) if a and tier(a)), 'none')
        counts[caller] += count
total = sum(counts.values())
for k, n in counts.most_common(40):
    print(f"{n:6d} {100*n/max(total,1):5.1f}%  {k}")
