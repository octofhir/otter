#!/usr/bin/env python3
"""Hot loops whose body holds a node that may run arbitrary code.

usage: loop_blockers.py <artifact-root> <launchprof-offsets.txt> [top]

Reads each optimized code object's optimized-ir.txt, finds loops (a block
marked `loop` and the blocks between it and a `JumpLoop` back to it, in
layout order), sums the self samples of their nodes (code-map regions by
operation index), and lists the generic nodes inside: kinds that can call or
enter the runtime and so write the whole heap, which keeps every load and
check of the loop from moving out of it.
"""
import collections, glob, json, re, sys

root, report = sys.argv[1], sys.argv[2]
top = int(sys.argv[3]) if len(sys.argv) > 3 else 20
BLOCKERS = ('LoadPropertyCached', 'StorePropertyCached', 'LoadKeyedCached', 'StoreKeyedCached',
            'Generic', 'CallJs', 'CallNative', 'StoreNamedProperty', 'LoadNamedProperty',
            'NewObjectLiteral', 'NewArrayLiteral', 'PrimitiveAdd', 'Call')
text = open(report).read().split('== self', 1)[1].split('== inclusive', 1)[0]
samples = collections.defaultdict(collections.Counter)
for line in text.splitlines():
    m = re.match(r'\s*(\d+)\s+JS:.*\[optimizing c(\d+)\]\+0x([0-9a-f]+)', line)
    if m:
        samples[int(m.group(2))][int(m.group(3), 16)] += int(m.group(1))
rows = []
for d in glob.glob(root + '/*optimizing*'):
    cid = int(re.search(r'-c(\d+)$', d).group(1))
    if cid not in samples:
        continue
    cmap = json.load(open(d + '/code-map.json'))
    by_op = collections.Counter()
    regions = [(r['startOffset'], r['endOffset'], r.get('operationIndex')) for r in cmap['regions']]
    for off, n in samples[cid].items():
        for s, e, op in regions:
            if s <= off < e and op is not None:
                by_op[op] += n
                break
    blocks, order, current = {}, [], None
    for line in open(d + '/optimized-ir.txt'):
        bm = re.match(r'b(\d+)( loop)? preds=', line)
        if bm:
            current = int(bm.group(1)); order.append(current)
            blocks[current] = {'loop': bool(bm.group(2)), 'nodes': []}
            continue
        nm = re.match(r'\s+v(\d+) = (\w+)(.*)', line)
        if nm and current is not None:
            blocks[current]['nodes'].append((int(nm.group(1)), nm.group(2), nm.group(3)))
    for i, b in enumerate(order):
        if not blocks[b]['loop']:
            continue
        body, end = [], None
        for b2 in order[i:]:
            body.append(b2)
            if any(k == 'JumpLoop' and f'BlockId({b})' in rest for _, k, rest in blocks[b2]['nodes']):
                end = b2
        if end is None:
            continue
        body = body[:body.index(end) + 1]
        total = sum(by_op[v] for b2 in body for v, _, _ in blocks[b2]['nodes'])
        blockers = [f'v{v} {k}{rest[:40]}' for b2 in body for v, k, rest in blocks[b2]['nodes']
                    if k in BLOCKERS]
        if blockers:
            rows.append((total, cid, b, blockers))
rows.sort(reverse=True)
for total, cid, b, blockers in rows[:top]:
    print(f'{total:5d} c{cid} b{b}: ' + '; '.join(blockers[:6]))
