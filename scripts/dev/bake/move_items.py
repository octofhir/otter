#!/usr/bin/env python3
"""Move named items of one impl region into another file's impl body.
usage: move_items.py <src> <start> <end> <dst> <keep-names-comma>
Items in [start,end] not in keep are cut from src and appended to dst
(before its final line, which must close the impl)."""
import re, subprocess, sys
src, start, end, dst, keep = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), sys.argv[4], set(sys.argv[5].split(','))
out = subprocess.run(['python3', 'scripts/dev/bake/split_items.py', src, str(start), str(end)], capture_output=True, text=True).stdout.split('\n')
items = [tuple(l.split()) for l in out if l.strip()]
d0 = open(dst).read().rstrip('\n').split('\n')
assert d0[-1] == '}', 'destination must end with the impl close'
lines = open(src).read().split('\n')
moved, cut = [], set()
for s, e, name in items:
    s, e = int(s), int(e)
    if name in keep:
        continue
    moved.append('\n'.join(lines[s - 1:e]))
    for k in range(s - 1, e):
        cut.add(k)
    # drop one blank separator after the item
    if e < len(lines) and lines[e].strip() == '':
        cut.add(e)
new_src = [l for k, l in enumerate(lines) if k not in cut]
open(src, 'w').write('\n'.join(new_src))
d = open(dst).read().rstrip('\n').split('\n')
assert d[-1] == '}', d[-1]
d = d[:-1] + ['\n\n'.join(moved), '}', '']
open(dst, 'w').write('\n'.join(d))
print(len(moved), 'items moved')
