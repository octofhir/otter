#!/usr/bin/env python3
"""Inclusive sample counts of the callees of every frame whose symbol
contains a substring, in a macOS `sample` capture's isolate thread.
usage: sample_children.py <sample.txt> <symbol-substring> [top]
"""
import re
import sys
from collections import Counter

target = sys.argv[2]
top = int(sys.argv[3]) if len(sys.argv) > 3 else 20
line_re = re.compile(r'^(?P<indent>[\s+!:|]*)(?P<count>\d+)\s+(?P<sym>.*)$')
stack = []
in_isolate = False
children = Counter()
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
    while stack and stack[-1][0] >= depth:
        stack.pop()
    sym = m.group('sym').split(' (in ')[0]
    if stack and target in stack[-1][1] and not any(target in s for _, s in stack[:-1]):
        children[sym[:130]] += int(m.group('count'))
    stack.append((depth, sym))
for name, n in children.most_common(top):
    print(f"{n:7d}  {name}")
