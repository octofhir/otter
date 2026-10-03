#!/usr/bin/env python3
"""Static census of graph-tier Generic nodes by bytecode opcode.

usage: generic_census.py <otter> <out-dir> <script> [top]
Runs the script with --jit-artifacts, then counts, over every optimizing
code object, the Generic nodes per opcode of the instruction they run.
"""
import collections, pathlib, re, subprocess, sys

otter, out, script = sys.argv[1], pathlib.Path(sys.argv[2]), sys.argv[3]
top = int(sys.argv[4]) if len(sys.argv) > 4 else 25
art = out / "art"
if not art.exists():
    subprocess.run([otter, "--timeout", "0", "run", f"--jit-artifacts={art}", script],
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=3600)
counts = collections.Counter()
for d in sorted(art.glob("jit-*-optimizing-*")):
    ops = {}
    for line in (d / "bytecode.txt").read_text().splitlines():
        m = re.match(r"(\d+) byte=\d+ (\w+)", line)
        if m:
            ops[int(m.group(1))] = m.group(2)
    for line in (d / "optimized-ir.txt").read_text().splitlines():
        m = re.search(r"Generic \{ pc: (\d+)", line)
        if m:
            counts[ops.get(int(m.group(1)), "?")] += 1
for op, n in counts.most_common(top):
    print(f"{n:6d}  {op}")
