#!/usr/bin/env python3
"""In-line instruction count per graph node of one code object's asm.txt,
in emission order, with its out-of-line size and prologue size.

usage: node_sizes.py <artifact-dir>
"""
import re, sys
regions, insns = [], []
for line in open(sys.argv[1] + "/asm.txt"):
    m = re.search(r"region kind=([\w-]+) range=\+0x([0-9a-f]+)\.\.\+0x([0-9a-f]+)(.*)", line)
    if m:
        op = re.search(r'tier-op="([^"]*)"', m.group(4))
        regions.append((int(m.group(2), 16), int(m.group(3), 16), m.group(1), op.group(1)[:90] if op else ""))
        continue
    m = re.match(r"^\+0x([0-9a-f]+): ", line)
    if m:
        insns.append(int(m.group(1), 16))
inl = [r for r in regions if r[2] == "instruction"]
first = min(r[0] for r in inl)
last = max(r[1] for r in inl)
def count(lo, hi):
    return sum(1 for o in insns if lo <= o < hi)
covered = 0
for lo, hi, kind, op in sorted(inl):
    n = count(lo, hi)
    covered += n
    print(f"{n:5d}  {op}")
print(f"prologue (before first node) {count(0, first)}; nodes {covered}; "
      f"between nodes {count(first, last) - covered}; out of line {count(last, 1 << 40)}")
