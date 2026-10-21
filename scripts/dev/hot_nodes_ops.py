#!/usr/bin/env python3
"""Self samples of optimized code per IR node, with the node's operation text.

usage: hot_nodes_ops.py <artifact-root> <launchprof-offsets.txt> [top] [kind-substring]

Like node_kind_prof.py, but keeps every node apart and prints its function,
logical PC and operation, so one hot node of a kind can be told from another.
"""
import collections, glob, json, os, re, sys

root, report = sys.argv[1], sys.argv[2]
top = int(sys.argv[3]) if len(sys.argv) > 3 else 40
needle = sys.argv[4] if len(sys.argv) > 4 else None
text = open(report).read().split('== self', 1)[1].split('== inclusive', 1)[0]
samples = collections.Counter()
for line in text.splitlines():
    m = re.match(r'\s*(\d+)\s+JS:(.*) \[optimizing c(\d+)\]\+0x([0-9a-f]+)', line)
    if m:
        samples[(int(m.group(3)), int(m.group(4), 16))] += int(m.group(1))
maps = {}
for manifest in glob.glob(os.path.join(root, '*optimizing*', 'manifest.json')):
    code = json.load(open(manifest))['codeObjectId']
    maps[code] = [r for r in json.load(open(os.path.join(os.path.dirname(manifest), 'code-map.json')))['regions']
                  if r['endOffset'] > r['startOffset']]
nodes = collections.Counter()
for (code, offset), count in samples.items():
    for region in maps.get(code, []):
        if region['startOffset'] <= offset < region['endOffset']:
            op = region.get('operation', region['kind'])
            nodes[(code, region.get('functionId'), region.get('logicalPc'), op[:150])] += count
            break
    else:
        nodes[(code, None, None, '<outside>')] += count
for (code, fid, pc, op), count in nodes.most_common():
    if needle and needle not in op:
        continue
    print(f'{count:6} c{code} f{fid} pc{pc} {op}')
    top -= 1
    if top == 0:
        break
