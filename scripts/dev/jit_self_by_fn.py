#!/usr/bin/env python3
"""Self samples of generated code on the otter-isolate thread of a macOS
`sample` capture, attributed to compiled functions through the run's
`--perf-basic-prof` map; totals per tier and the top functions.
usage: jit_self_by_fn.py <sample.txt> <perf.map> [top]
"""
import bisect
import re
import sys
from collections import Counter

top = int(sys.argv[3]) if len(sys.argv) > 3 else 30
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
    return '(unmapped)'


line_re = re.compile(r'^(?P<indent>[\s+!:|]*)(?P<count>\d+)\s+(?P<sym>.*)$')
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
    addr = None
    if sym.startswith('???'):
        a = re.search(r'\[0x([0-9a-f]+)\]', sym)
        addr = int(a.group(1), 16) if a else None
    nodes.append([depth, int(m.group('count')), addr, 0])
    if parent is not None:
        nodes[parent][3] += int(m.group('count'))
    stack.append(len(nodes) - 1)

by_fn = Counter()
by_tier = Counter()
total = 0
for depth, count, addr, child in nodes:
    self_n = count - child
    if self_n <= 0:
        continue
    total += self_n
    if addr is None:
        continue
    name = name_of(addr)
    tier = re.search(r'\[(\w+)', name)
    by_tier[tier.group(1) if tier else name] += self_n
    by_fn[name] += self_n
print(f'isolate self samples {total}')
for tier, n in by_tier.most_common():
    print(f'{n:6d} {100*n/total:5.1f}%  {tier}')
print('-- top functions')
for name, n in by_fn.most_common(top):
    print(f'{n:6d} {100*n/total:5.1f}%  {name}')
