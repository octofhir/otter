#!/usr/bin/env python3
"""Print a `--jit-artifacts` code object's assembly grouped under the graph
node regions of its code map, with each region's instruction count.

usage: asm_by_node.py <artifact-dir> [operation-substring]
"""
import json, re, sys

art = sys.argv[1]
want = sys.argv[2] if len(sys.argv) > 2 else ""
regions = json.load(open(f"{art}/code-map.json"))["regions"]
insns = {}
for line in open(f"{art}/asm.txt"):
    m = re.match(r"\+0x([0-9a-f]+): [0-9a-f]+\s+(.*)$", line.rstrip())
    if m:
        insns[int(m.group(1), 16)] = m.group(2)
for r in regions:
    op = r.get("operation", r["kind"])
    if want and want not in op:
        continue
    body = [(o, insns[o]) for o in sorted(insns) if r["startOffset"] <= o < r["endOffset"]]
    print(f"== b{r.get('block')} {op}  [{len(body)}]")
    for o, text in body:
        print(f"   +{o:05x} {text}")
