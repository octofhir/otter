#!/usr/bin/env python3
"""Self samples of the otter-isolate thread attributed to the first native
frame below the innermost generated-code frame of a macOS `sample` capture:
what generated code spends its runtime calls on. Trampoline frames are
skipped. Samples with no generated frame above them are reported apart.
usage: jit_callees.py <sample.txt> [top]
"""
import re
import sys
from collections import Counter

TRAMPOLINES = ('otter_native_invoke', 'otter_native_reservation', 'call_native_entry', 'host_call_entry')
top = int(sys.argv[2]) if len(sys.argv) > 2 else 40
line_re = re.compile(r'^(?P<indent>[\s+!:|]*)(?P<count>\d+)\s+(?P<sym>.*)$')


def clean(sym):
    name = re.sub(r'\s+\(in .*$', '', sym)
    return re.sub(r'\s+\+\s+\d+.*$', '', name)[:110]


nodes = []  # (depth, count, name, is_jit, parent_index)
in_isolate = False
stack = []
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
    nodes.append([depth, int(m.group('count')), clean(sym), sym.startswith('???'), parent, 0])
    if parent is not None:
        nodes[parent][5] += int(m.group('count'))
    stack.append(len(nodes) - 1)

counts = Counter()
outside = 0
for i, (depth, count, name, is_jit, parent, child_sum) in enumerate(nodes):
    self_n = count - child_sum
    if self_n <= 0:
        continue
    if is_jit:
        counts['(generated code)'] += self_n
        continue
    # Walk up to the innermost generated frame, remembering the frame below it.
    below = i
    cur = parent
    while cur is not None and not nodes[cur][3]:
        below = cur
        cur = nodes[cur][4]
    if cur is None:
        outside += self_n
        continue
    # Skip trampolines between the generated frame and the real callee.
    path = []
    j = i
    while j != below:
        path.append(j)
        j = nodes[j][4]
    path.append(below)
    path.reverse()
    pick = next((k for k in path if not any(t in nodes[k][2] for t in TRAMPOLINES)), below)
    counts[nodes[pick][2]] += self_n
total = sum(counts.values()) + outside
print(f'isolate self samples: {total}  (no generated frame above: {outside})')
for name, n in counts.most_common(top):
    print(f'{n:6d} {100*n/total:5.1f}%  {name}')
