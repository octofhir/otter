#!/usr/bin/env python3
"""Sample-weighted share of stack-slot moves in generated code: each graph
node's self samples split evenly over its region's instructions, summed for
the `[sp, ...]` loads and stores (spills, reloads, outgoing arguments).

usage: spill_weight.py <jit_hot_regions output> <artifacts-dir>
"""
import collections, glob, re, sys

hot = {}
for line in open(sys.argv[1]):
    m = re.match(r"\s*(\d+)\s+[\d.]+%\s+JS:.*\[(?:optimizing|template) c(\d+)\] (v\d+) ", line)
    if m:
        hot[(m.group(2), m.group(3))] = hot.get((m.group(2), m.group(3)), 0) + int(m.group(1))
total = spill = 0.0
kinds = collections.Counter()
cache = {}
for (code, node), samples in hot.items():
    if code not in cache:
        regions = collections.defaultdict(list)
        current = None
        for path in glob.glob(f"{sys.argv[2]}/*-c{code}/asm.txt"):
            for line in open(path):
                m = re.search(r'region kind=\w+ .*tier-op="(v\d+) ', line)
                if m:
                    current = m.group(1)
                    continue
                if line.startswith("L"):
                    continue
                m = re.match(r"\+0x[0-9a-f]+: [0-9a-f]+\s+(.*)", line)
                if m and current:
                    regions[current].append(m.group(1))
        cache[code] = regions
    instrs = cache[code].get(node, [])
    if not instrs:
        continue
    each = samples / len(instrs)
    for ins in instrs:
        total += each
        if re.search(r"\[sp", ins) and re.match(r"(ldr|str|ldp|stp|ldur|stur)", ins):
            spill += each
            kinds[ins.split()[0]] += each
print(f"weighted instructions {total:.0f}, stack-slot moves {spill:.0f} = {100 * spill / max(total, 1):.1f}%")
print(dict((k, round(v)) for k, v in kinds.items()))
