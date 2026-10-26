#!/usr/bin/env python3
"""Isolate-thread samples of a macOS `sample` capture credited to the first
Rust frame each generated-code frame called (`--perf-basic-prof` map): which
runtime entries compiled JavaScript leaves for, inclusive of everything they
do. Samples whose innermost frame is generated code count as
`(generated code)`; samples with no generated frame as `(no JS frame)`.
usage: runtime_entry_by_fn.py <sample.txt> <perf.map> [top]
"""
import bisect
import re
import sys
from collections import Counter

top = int(sys.argv[3]) if len(sys.argv) > 3 else 40
ranges = []
for line in open(sys.argv[2]):
    parts = line.rstrip('\n').split(' ', 2)
    if len(parts) == 3:
        ranges.append((int(parts[0], 16), int(parts[1], 16), parts[2]))
ranges.sort()
starts = [r[0] for r in ranges]


def is_js(sym):
    a = re.search(r'\[0x([0-9a-f]+)\]', sym)
    if not sym.startswith('???') or not a:
        return False
    addr = int(a.group(1), 16)
    i = bisect.bisect_right(starts, addr) - 1
    return i >= 0 and ranges[i][0] <= addr < ranges[i][0] + ranges[i][1]


line_re = re.compile(r'^(?P<indent>[\s+!:|]*)(?P<count>\d+)\s+(?P<sym>.*)$')
# Each node: depth, count, attributed entry, child count, js-frame
nodes = []
stack = []
in_isolate = False
for line in open(sys.argv[1]):
    if 'Thread_' in line:
        in_isolate = 'otter-isolate' in line
        stack = []
        continue
    if not in_isolate:
        continue
    m = line_re.match(line.rstrip('\n'))
    if not m:
        continue
    depth = len(m.group('indent'))
    while stack and nodes[stack[-1]][0] >= depth:
        stack.pop()
    parent = stack[-1] if stack else None
    sym = m.group('sym')
    js = is_js(sym)
    short = sym.split(' (in ')[0][:110]
    if js:
        entry = '(generated code)'
    elif parent is not None and nodes[parent][4]:
        entry = short
    else:
        entry = nodes[parent][2] if parent is not None else '(no JS frame)'
    nodes.append([depth, int(m.group('count')), entry, 0, js])
    if parent is not None:
        nodes[parent][3] += int(m.group('count'))
    stack.append(len(nodes) - 1)

by_entry = Counter()
total = 0
for depth, count, entry, child, js in nodes:
    own = count - child
    if own > 0:
        by_entry[entry] += own
        total += own
print(f"isolate samples {total}")
for name, n in by_entry.most_common(top):
    print(f"{n:7d} {100.0 * n / total:5.1f}%  {name}")
