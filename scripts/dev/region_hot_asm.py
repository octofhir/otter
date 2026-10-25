#!/usr/bin/env python3
"""Hot instructions inside the optimized-code regions of one node kind.

usage: region_hot_asm.py <out-dir> <perf-map> <kind> [regions]

<out-dir> is a `sample_kind_prof.py` output directory (its `sample.txt` and
`art/` bundle from the same run) and <perf-map> that run's
`/tmp/perf-<pid>.map`. Every self sample in an optimizing-tier region whose
operation is <kind> (or, for `<outside>`, in the gap between node regions)
is credited to the instruction holding it; the hottest
regions are printed with their per-instruction samples, then the kind's
samples by offset from the region start.
"""
import bisect
import collections
import glob
import os
import re
import sys

out, perf_map, want = sys.argv[1], sys.argv[2], sys.argv[3]
show = int(sys.argv[4]) if len(sys.argv) > 4 else 3
art = os.path.join(out, "art")
maps = []
for line in open(perf_map):
    address, size, name = line.split(" ", 2)
    maps.append((int(address, 16), int(size, 16), name.strip()))
maps.sort()
starts = [m[0] for m in maps]
code_cache = {}


def code(code_id):
    if code_id not in code_cache:
        regions, insns = [], {}
        for path in glob.glob(os.path.join(art, f"*-c{code_id}")):
            for line in open(os.path.join(path, "asm.txt")):
                m = re.search(r'range=\+0x([0-9a-f]+)\.\.\+0x([0-9a-f]+).*?tier-op="([^"]*)"', line)
                if m:
                    kind = re.sub(r"^v\d+ ", "", m.group(3))
                    regions.append((int(m.group(1), 16), int(m.group(2), 16), kind))
                m = re.match(r"\+0x([0-9a-f]+): (.*)$", line.rstrip())
                if m:
                    insns[int(m.group(1), 16)] = m.group(2)
        code_cache[code_id] = (sorted(regions), insns)
    return code_cache[code_id]


text = open(os.path.join(out, "sample.txt")).read()
graph = text.split("Call graph:", 1)[1].split("Total number in stack", 1)[0]
thread = next(t for t in re.split(r"\n    (?=\d+ Thread_)", graph) if "otter-isolate" in t.split("\n", 1)[0])
rows = []
for line in thread.splitlines():
    m = re.match(r"^([\s+!:|]*)(\d+) (.*)$", line)
    if m:
        rows.append((len(m.group(1)), int(m.group(2)), m.group(3)))
per_region = collections.defaultdict(collections.Counter)
by_offset = collections.Counter()
total = 0.0
for i, (depth, count, frame) in enumerate(rows):
    children, j, first = 0, i + 1, None
    while j < len(rows) and rows[j][0] > depth:
        if first is None:
            first = rows[j][0]
        if rows[j][0] == first:
            children += rows[j][1]
        j += 1
    own = count - children
    if own <= 0 or "???" not in frame or "[" not in frame:
        continue
    addresses = [int(a, 16) for a in re.findall(r"0x([0-9a-f]+)", frame.split("[", 1)[1])]
    share = own / len(addresses)
    for address in addresses:
        k = bisect.bisect_right(starts, address) - 1
        if k < 0 or address >= maps[k][0] + maps[k][1]:
            continue
        m = re.search(r"\[optimizing c(\d+)\]", maps[k][2])
        if not m:
            continue
        offset = (address - maps[k][0]) & ~3
        regions, _ = code(m.group(1))
        hit = [r for r in regions if r[0] <= offset < r[1]]
        if want == "<outside>" and not hit:
            # Code outside every node region, credited to the gap holding it.
            before = [r[1] for r in regions if r[1] <= offset]
            after = [r[0] for r in regions if r[0] > offset]
            hit = [(max(before, default=0), min(after, default=offset + 4), "<outside>")]
        if not hit or not hit[-1][2].startswith(want):
            continue
        region = hit[-1]
        per_region[(m.group(1), region[0], region[1], region[2])][offset] += share
        by_offset[offset - region[0]] += share
        total += share
print(f"{want}: {total:.0f} samples in {len(per_region)} regions")
ranked = sorted(per_region.items(), key=lambda kv: -sum(kv[1].values()))
for (code_id, start, end, kind), hits in ranked[:show]:
    _, insns = code(code_id)
    print(f"\n== c{code_id} +0x{start:x}..+0x{end:x} {kind}: {sum(hits.values()):.0f}")
    for offset in range(start, end, 4):
        if offset in insns:
            mark = f"{hits[offset]:5.0f}" if hits.get(offset) else "     "
            print(f"{mark}  +0x{offset - start:03x} {insns[offset]}")
print("\n== by offset from region start")
for offset, value in sorted(by_offset.items(), key=lambda kv: -kv[1])[:30]:
    print(f"{value:6.0f}  +0x{offset:03x}")
