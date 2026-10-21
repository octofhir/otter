#!/usr/bin/env python3
"""Print the machine code of graph nodes whose operation matches a substring.

usage: node_asm.py <bundle-dir> <operation-substring> [limit]
Reads the bundle's code-map.json regions and asm.txt listing.
"""
import json, pathlib, sys

bundle = pathlib.Path(sys.argv[1])
needle = sys.argv[2]
limit = int(sys.argv[3]) if len(sys.argv) > 3 else 3
regions = json.loads((bundle / "code-map.json").read_text())["regions"]
listing = []
for line in (bundle / "asm.txt").read_text().splitlines():
    head = line.split(":", 1)[0].strip()
    try:
        listing.append((int(head, 16), line))
    except ValueError:
        continue
shown = 0
for region in regions:
    if needle not in region.get("operation", "") or region["endOffset"] <= region["startOffset"]:
        continue
    start, end = region["startOffset"], region["endOffset"]
    print(f"== {region['operation']} [{start:#x}, {end:#x}) {(end - start) // 4} words")
    for offset, line in listing:
        if start <= offset < end:
            print(line)
    shown += 1
    if shown >= limit:
        break
