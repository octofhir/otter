#!/usr/bin/env python3
"""Inclusive samples of each shared routine credited to the calling graph
node: the caller frame's return address resolved to a perf-map code object
and then to its `asm.txt` region.

usage: routine_sites.py <sample.txt> <perf.map> <jit-artifacts-dir> [top]
The run must have written both `--perf-basic-prof` and `--jit-artifacts`.
"""
import bisect, collections, glob, re, sys

sample, perf_map, artifacts = sys.argv[1:4]
top = int(sys.argv[4]) if len(sys.argv) > 4 else 8
text = open(sample).read()
graph = text.split("Call graph:", 1)[1].split("Total number in stack", 1)[0]
thread = graph.split("otter-isolate", 1)[1]
thread = re.split(r"\n    \d+ Thread_", thread, maxsplit=1)[0]
entries = []
for line in thread.splitlines():
    m = re.match(r"^([\s+!:|]*?)(\d+) (.*)$", line)
    if m:
        entries.append((len(m.group(1)), int(m.group(2)), m.group(3)))
codes = []
for line in open(perf_map):
    start, size, name = line.rstrip("\n").split(" ", 2)
    codes.append((int(start, 16), int(size, 16), name))
codes.sort()
starts = [c[0] for c in codes]


def look(frame):
    m = re.search(r"\?\?\?.*\[0x([0-9a-f]+)\]", frame)
    if not m:
        return None, None
    pc = int(m.group(1), 16)
    i = bisect.bisect_right(starts, pc) - 1
    if i >= 0 and pc < codes[i][0] + codes[i][1]:
        return codes[i][2], pc - codes[i][0]
    return None, None


def region(name, offset):
    m = re.search(r"\bc(\d+)\]", name)
    if not m:
        return "?"
    for path in glob.glob(f"{artifacts}/*-c{m.group(1)}/asm.txt"):
        for line in open(path):
            r = re.search(
                r"region kind=\w+ range=\+0x([0-9a-f]+)\.\.\+0x([0-9a-f]+).*?tier-op=\"(v\d+ [^\"]{0,90})",
                line,
            )
            if r and int(r.group(1), 16) <= offset - 4 < int(r.group(2), 16):
                return r.group(3)
    return "(no region)"


by_routine = collections.defaultdict(collections.Counter)
stack = []
for depth, count, frame in entries:
    while stack and stack[-1][0] >= depth:
        stack.pop()
    stack.append((depth, frame))
    name, _ = look(frame)
    if name and name.startswith("routine") and len(stack) > 1:
        caller, offset = look(stack[-2][1])
        if caller and not caller.startswith("routine"):
            by_routine[name][(caller, offset)] += count
for routine, sites in sorted(by_routine.items(), key=lambda kv: -sum(kv[1].values())):
    print(f"== {routine} {sum(sites.values())}")
    for (caller, offset), count in sites.most_common(top):
        print(f"  {count:5d} {caller} {region(caller, offset)[:110]}")
