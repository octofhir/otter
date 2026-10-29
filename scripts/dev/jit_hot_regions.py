#!/usr/bin/env python3
"""Self samples of generated code, credited to the graph node whose asm
region holds each sampled pc.

usage: jit_hot_regions.py <sample.txt> <perf.map> <jit-artifacts-dir> [top]
The run must have written both `--perf-basic-prof` and `--jit-artifacts`, so
a code object's perf-map name `[... cN]` matches its artifact directory
`*-cN` and an offset into it names an `asm.txt` region. A pc before the first
region is `(prologue)`, past the last one `(out of line)` (deferred paths,
exits, the epilogue); a pc in no code object (a shared routine) is credited as `routine <- ` the region of the
nearest caller frame that is in one.
"""
import bisect, collections, glob, os, re, sys

sample, perf_map, artifacts = sys.argv[1:4]
top = int(sys.argv[4]) if len(sys.argv) > 4 else 30

text = open(sample).read()
graph = text.split("Call graph:", 1)[1].split("Total number in stack", 1)[0]
thread = graph.split("otter-isolate", 1)[1].split("\n    ", 1)[1]
thread = re.split(r"\n    \d+ Thread_", thread, maxsplit=1)[0]
entries = []
for line in thread.splitlines():
    m = re.match(r"^([\s+!:|]*?)(\d+) (.*)$", line)
    if m:
        entries.append((len(m.group(1)), int(m.group(2)), m.group(3)))
def pc_of(name):
    m = re.search(r"\?\?\?.*\[0x([0-9a-f]+)\]", name)
    return int(m.group(1), 16) if m else None

leaves = collections.Counter()
stack = []
for i, (depth, count, name) in enumerate(entries):
    while stack and stack[-1][0] >= depth:
        stack.pop()
    stack.append((depth, name))
    children, j = 0, i + 1
    while j < len(entries) and entries[j][0] > depth:
        if entries[j][0] == entries[i + 1][0]:
            children += entries[j][1]
        j += 1
    own = count - children
    pc = pc_of(name)
    if own > 0 and pc is not None:
        callers = tuple(p for p in (pc_of(n) for _, n in reversed(stack[:-1])) if p is not None)
        leaves[(pc, callers)] += own

codes = []
for line in open(perf_map):
    start, size, name = line.split(" ", 2)
    m = re.search(r"\bc(\d+)\]", name)
    codes.append((int(start, 16), int(size, 16), name.strip(), m and m.group(1)))
codes.sort()
starts = [c[0] for c in codes]

regions = {}
def regions_of(code_id):
    if code_id not in regions:
        found = []
        for path in glob.glob(os.path.join(artifacts, f"*-c{code_id}", "asm.txt")):
            for line in open(path):
                m = re.search(r"region kind=(\w+) range=\+0x([0-9a-f]+)\.\.\+0x([0-9a-f]+)(.*)", line)
                if m:
                    op = re.search(r'tier-op="(v\d+ \w+)', m.group(4))
                    label = op.group(1) if op else f"[{m.group(1)}]"
                    found.append((int(m.group(2), 16), int(m.group(3), 16), label))
        regions[code_id] = sorted(found)
    return regions[code_id]

def region_of(pc):
    i = bisect.bisect_right(starts, pc) - 1
    if i < 0 or pc >= codes[i][0] + codes[i][1]:
        return None
    start, _, name, code_id = codes[i]
    # A caller frame's pc is a return address: credit the call before it.
    offset = pc - start
    found = regions_of(code_id)
    for lo, hi, region in found:
        if lo <= offset < hi:
            return f"{name} {region}"
    if found and offset < found[0][0]:
        return f"{name} (prologue)"
    if found and offset >= max(hi for _, hi, _ in found):
        return f"{name} (out of line)"
    return f"{name} (no region)"

by_node = collections.Counter()
total = sum(leaves.values())
for (pc, callers), count in leaves.items():
    label = region_of(pc)
    if label is None:
        caller = next((r for r in (region_of(c - 4) for c in callers) if r), None)
        label = f"routine <- {caller}" if caller else "(unmapped)"
    by_node[label] += count
print(f"generated self samples: {total}")
for key, count in by_node.most_common(top):
    print(f"{count:6d} {100.0 * count / max(total, 1):5.1f}%  {key}")
