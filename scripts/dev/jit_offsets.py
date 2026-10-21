#!/usr/bin/env python3
"""Attribute a function's self samples to its graph operations.

usage: jit_offsets.py <launchprof-report> <artifact-dir> <perf-map name>
The report is launchprof.py output run with OFFSETS=1 (and a large TOP);
the artifact directory is the matching --jit-artifacts bundle of the same
run. Prints the self samples of `<name>` grouped by the code region (IR
operation, source function and logical PC) each sampled offset falls in.
"""
import bisect, collections, re, sys

report, bundle, name = sys.argv[1:4]
regions = []
for line in open(f"{bundle}/asm.txt"):
    m = re.search(r"region kind=(\S+) range=\+0x([0-9a-f]+)\.\.\+0x([0-9a-f]+)", line)
    if m:
        op = re.search(r'tier-op="([^"]*)"', line)
        fn = re.search(r"function=(\d+)", line)
        pc = re.search(r" pc=(\d+)", line)
        regions.append((int(m.group(2), 16), int(m.group(3), 16), m.group(1),
                        fn.group(1) if fn else "", pc.group(1) if pc else "",
                        op.group(1)[:80] if op else ""))
regions.sort()
starts = [r[0] for r in regions]
self_section = open(report).read().split("== inclusive")[0]
counts, total = collections.Counter(), 0
pattern = re.compile(r"\s*(\d+)\s+" + re.escape(name) + r"\+0x([0-9a-f]+)")
for line in self_section.splitlines():
    m = pattern.match(line)
    if not m:
        continue
    samples, offset = int(m.group(1)), int(m.group(2), 16)
    total += samples
    i = bisect.bisect_right(starts, offset) - 1
    key = regions[i][2:] if i >= 0 and offset < regions[i][1] + 4 else ("?", "", "", hex(offset))
    counts[key] += samples
print(f"{total} self samples")
for key, samples in counts.most_common(int(sys.argv[4]) if len(sys.argv) > 4 else 40):
    print(f"{samples:6d}  {key}")
