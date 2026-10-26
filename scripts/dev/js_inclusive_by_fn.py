#!/usr/bin/env python3
"""Isolate-thread samples of a macOS `sample` capture credited to the
innermost compiled JavaScript function on each stack (`--perf-basic-prof`
map): what each generated function costs including the runtime it calls.
Samples with no generated frame on the stack count as `(no JS frame)`.
usage: js_inclusive_by_fn.py <sample.txt> <perf.map> [top] [function-substring]
With a function substring, lists the leaf symbols of the samples credited to
the matching functions instead.
"""
import bisect
import re
import sys
from collections import Counter

top = int(sys.argv[3]) if len(sys.argv) > 3 else 40
focus = sys.argv[4] if len(sys.argv) > 4 else None
ranges = []
for line in open(sys.argv[2]):
    parts = line.rstrip('\n').split(' ', 2)
    if len(parts) == 3:
        ranges.append((int(parts[0], 16), int(parts[1], 16), parts[2]))
ranges.sort()
starts = [r[0] for r in ranges]


def name_of(addr):
    i = bisect.bisect_right(starts, addr) - 1
    if i >= 0 and ranges[i][0] <= addr < ranges[i][0] + ranges[i][1]:
        return ranges[i][2]
    return None


line_re = re.compile(r'^(?P<indent>[\s+!:|]*)(?P<count>\d+)\s+(?P<sym>.*)$')
# Each node: depth, count, js-name-of-innermost-js-frame-so-far, child-count
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
    js = nodes[parent][2] if parent is not None else None
    if sym.startswith('???'):
        a = re.search(r'\[0x([0-9a-f]+)\]', sym)
        name = name_of(int(a.group(1), 16)) if a else None
        if name:
            js = name
    nodes.append([depth, int(m.group('count')), js, 0, sym.split(' (in ')[0][:110]])
    if parent is not None:
        nodes[parent][3] += int(m.group('count'))
    stack.append(len(nodes) - 1)

by_fn = Counter()
total = 0
for depth, count, js, child, sym in nodes:
    own = count - child
    if own > 0:
        name = js or '(no JS frame)'
        if focus is not None:
            if focus not in name:
                continue
            name = sym
        by_fn[name] += own
        total += own
print(f"isolate samples {total}")
for name, n in by_fn.most_common(top):
    print(f"{n:7d} {100.0 * n / total:5.1f}%  {name}")
