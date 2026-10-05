#!/usr/bin/env python3
"""Print the assembly of one median-sized site of an operation kind in a JIT artifact.

Usage: scripts/dev/template_site_asm.py <artifact-dir> <OperationKind>
"""
import json
import re
import sys

art, kind = sys.argv[1], sys.argv[2]
code_map = json.load(open(f"{art}/code-map.json"))
regions = [r for r in code_map["regions"]
           if r["kind"] == "instruction" and r["operation"].split(" ")[0] == kind]
if not regions:
    raise SystemExit(f"no {kind} site")
region = sorted(regions, key=lambda r: r["endOffset"] - r["startOffset"])[len(regions) // 2]
print(region["operation"], region["endOffset"] - region["startOffset"], "bytes")
for line in open(f"{art}/asm.txt"):
    match = re.match(r"\+0x([0-9a-f]+): [0-9a-f]+  (.*)", line)
    if not match:
        continue
    offset = int(match.group(1), 16)
    if region["startOffset"] <= offset < region["endOffset"]:
        print("  " + match.group(2))
    elif offset >= region["endOffset"]:
        break
