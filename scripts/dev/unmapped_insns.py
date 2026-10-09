#!/usr/bin/env python3
"""Show the instructions that self samples outside every code-map region hit.

usage: unmapped_insns.py <artifacts-dir> <self-offsets.txt> [top]
Inputs as for hot_nodes.py; prints sampled instructions (offset text dropped)
aggregated across code objects, so prologue/epilogue sequences surface.
"""
import collections, json, pathlib, re, sys

art = pathlib.Path(sys.argv[1])
top = int(sys.argv[3]) if len(sys.argv) > 3 else 40
bundles = {}
for directory in art.iterdir():
    match = re.match(r"jit-\d+-(\w+)-f(\d+)-c(\d+)$", directory.name)
    if match:
        bundles[int(match.group(3))] = directory
cache = {}
def load(code):
    if code not in cache:
        path = bundles.get(code)
        if path is None:
            cache[code] = ([], {})
        else:
            regions = [r for r in json.loads((path / "code-map.json").read_text())["regions"]
                       if r.get("endOffset", 0) > r.get("startOffset", 0)]
            listing = {}
            for line in (path / "asm.txt").read_text().splitlines():
                head, _, rest = line.partition(":")
                try:
                    listing[int(head.strip(), 16)] = rest.strip()
                except ValueError:
                    pass
            cache[code] = (regions, listing)
    return cache[code]
insns = collections.Counter()
for line in open(sys.argv[2]):
    match = re.match(r"\s*(\d+)\s+JS:(.*) \[(\w+) c(\d+)\]\+0x([0-9a-f]+)", line)
    if not match or match.group(3) != "optimizing":
        continue
    count, code, offset = int(match.group(1)), int(match.group(4)), int(match.group(5), 16)
    regions, listing = load(code)
    if any(r["startOffset"] <= offset < r["endOffset"] for r in regions):
        continue
    pc = offset & ~3
    text = listing.get(pc, "?")
    # neighbourhood mnemonic of the previous instruction (the sampled return address is +1)
    prev = listing.get(pc - 4, "?")
    text = re.sub(r"^[0-9a-f]{8}\s+", "", text)
    prev = re.sub(r"^[0-9a-f]{8}\s+", "", prev)
    text = re.sub(r"L[0-9a-f]{8}", "L", text)
    prev = re.sub(r"L[0-9a-f]{8}", "L", prev)
    insns[f"{prev:40} | {text}"] += count
for key, count in insns.most_common(top):
    print(f"{count:6}  {key}")
