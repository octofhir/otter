#!/usr/bin/env python3
"""Classify optimized-code samples outside every IR node region.

usage: outside_prof.py <artifact-root> <launchprof-offsets.txt>

For each `JS:... [optimizing cN]+0x..` self row of a launchprof
`OFFSETS=1 TOP=100000` report, credits samples that fall outside every
code-map instruction region of code object N to: the generic entry (before
the call entry), the direct-call prologue (call entry up to the first
region), out-of-line regions, or the tail (exits, veneers, deferred paths).
"""
import collections, glob, json, re, sys

root, report = sys.argv[1], sys.argv[2]
maps = {}
for d in glob.glob(root + '/*optimizing*'):
    cid = int(re.search(r'-c(\d+)$', d).group(1))
    m = json.load(open(d + '/code-map.json'))
    regs = sorted((r['startOffset'], r['endOffset'], r['kind']) for r in m['regions'])
    maps[cid] = (m['callEntryOffset'], regs)
text = open(report).read().split('== self', 1)[1].split('== inclusive', 1)[0]
cnt = collections.Counter()
for line in text.splitlines():
    mm = re.match(r'\s*(\d+)\s+JS:.*\[optimizing c(\d+)\]\+0x([0-9a-f]+)', line)
    if not mm or int(mm.group(2)) not in maps:
        continue
    n, cid, off = int(mm.group(1)), int(mm.group(2)), int(mm.group(3), 16)
    ce, regs = maps[cid]
    inside = [r for r in regs if r[0] <= off < r[1]]
    if inside:
        if inside[0][2] == 'out-of-line':
            cnt['out-of-line'] += n
        continue
    first = regs[0][0] if regs else 0
    last = regs[-1][1] if regs else 0
    if off < ce:
        cnt['generic entry'] += n
    elif off < first:
        cnt['call-entry prologue'] += n
    elif off >= last:
        cnt['tail (exits, veneers, deferred)'] += n
    else:
        cnt['between regions'] += n
for k, v in cnt.most_common():
    print(f'{v:6d}  {k}')
