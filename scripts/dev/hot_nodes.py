#!/usr/bin/env python3
"""Attribute self samples of generated code to graph nodes.

usage: hot_nodes.py <artifacts-dir> <self-offsets.txt> [top]
<self-offsets.txt> is the `== self` section of an OFFSETS=1 launchprof run
(lines `N  JS:<name> [<tier> c<id>]+0x<offset>`); <artifacts-dir> is a
`--jit-artifacts` capture of the same deterministic workload, so code object
ids agree. Prints, over every captured code object, samples per node kind
(`operation` of the code-map region holding the sampled offset), then the
hottest individual nodes.
"""
import collections, json, pathlib, re, sys

art = pathlib.Path(sys.argv[1])
top = int(sys.argv[3]) if len(sys.argv) > 3 else 30
bundles = {}
for directory in art.iterdir():
    match = re.match(r"jit-\d+-(\w+)-f(\d+)-c(\d+)$", directory.name)
    if match:
        bundles[int(match.group(3))] = directory

regions_by_code = {}
def regions(code):
    if code not in regions_by_code:
        path = bundles.get(code)
        data = json.loads((path / "code-map.json").read_text()) if path else {"regions": []}
        regions_by_code[code] = [r for r in data["regions"] if r.get("endOffset", 0) > r.get("startOffset", 0)]
    return regions_by_code[code]

kinds = collections.Counter()
nodes = collections.Counter()
for line in open(sys.argv[2]):
    match = re.match(r"\s*(\d+)\s+JS:(.*) \[(\w+) c(\d+)\]\+0x([0-9a-f]+)", line)
    if not match:
        continue
    count, name, tier, code, offset = int(match.group(1)), match.group(2), match.group(3), int(match.group(4)), int(match.group(5), 16)
    hit = None
    for region in regions(code):
        if region["startOffset"] <= offset < region["endOffset"]:
            if hit is None or region["endOffset"] - region["startOffset"] < hit["endOffset"] - hit["startOffset"]:
                hit = region
    label = hit.get("operation") or hit.get("kind") if hit else "unmapped"
    kind = re.sub(r"^v\d+ ", "", label)
    kind = re.split(r"[ ({]", kind, maxsplit=1)[0]
    kinds[f"{tier}:{kind}"] += count
    nodes[f"c{code} {name} {label}"] += count
total = sum(kinds.values())
print(f"samples {total}")
for kind, count in kinds.most_common(top):
    print(f"{count:6} {100 * count / total:5.1f}%  {kind}")
print("== nodes")
for node, count in nodes.most_common(top):
    print(f"{count:6}  {node[:150]}")
