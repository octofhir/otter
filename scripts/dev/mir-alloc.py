#!/usr/bin/env python3
"""Pair Machine IR operands with their allocated locations.

Usage: scripts/dev/mir-alloc.py <optimized-ir.txt> [first-inst] [last-inst]

Prints each instruction as `iN Opcode v<value>@<location>(role) ...` with the
regalloc edits placed before it, so a reader can follow one value through
moves, spills and safepoints without cross-referencing the two listings.
"""
import re
import sys

text = open(sys.argv[1]).read()
lo = int(sys.argv[2]) if len(sys.argv) > 2 else 0
hi = int(sys.argv[3]) if len(sys.argv) > 3 else 10**9

def reg(s):
    s = re.sub(r"PhysicalRegister \{ class: Int, encoding: (\d+) \}", r"x\1", s)
    s = re.sub(r"PhysicalRegister \{ class: Float, encoding: (\d+) \}", r"d\1", s)
    s = re.sub(r"Register\(([xd]\d+)\)", r"\1", s)
    return s.replace("Stack(", "S(")

insts = {}
blocks = {}
for line in text.split("\n"):
    m = re.match(r"  i(\d+) (.*?) \[(.*)\] clobbers=", line)
    if m:
        ops = re.findall(r"value: MachineValue\((\d+)\), constraint: [^,]*(?:\([^)]*\)\))?[^,]*, role: (\w+), timing: \w+, purpose: (\w+)", m.group(3))
        insts[int(m.group(1))] = (m.group(2)[:60], ops)
    b = re.match(r"(b\d+) i(\d+)\.\.i(\d+) (.*)", line)
    if b:
        blocks[int(b.group(2))] = b.group(1) + " " + b.group(4)[:160]
alloc = {}
edits = {}
section = text.split("\nallocation ", 1)[1]
for line in section.split("\n"):
    m = re.match(r"i(\d+) \[(.*)\]$", line)
    if m:
        alloc[int(m.group(1))] = [reg(x.strip()) for x in re.findall(r"(Register\(PhysicalRegister \{[^}]*\}\)|Stack\(\d+\))", m.group(2))]
    e = re.match(r"edit AllocationEdit \{ point: (\w+)\(MachineInstructionId\((\d+)\)\), from: (.*), to: (.*) \}", line)
    if e:
        edits.setdefault((e.group(1), int(e.group(2))), []).append(reg(e.group(3)) + "->" + reg(e.group(4)))
for i in sorted(insts):
    if not lo <= i <= hi:
        continue
    if i in blocks:
        print(blocks[i])
    if ("Before", i) in edits:
        print("    edits:", ", ".join(edits[("Before", i)]))
    name, ops = insts[i]
    locs = alloc.get(i, [])
    parts = []
    for k, (v, role, purpose) in enumerate(ops):
        loc = locs[k] if k < len(locs) else "?"
        tag = {"Input": "", "Output": "=", "FrameState": "fs", "TaggedRoot": "root", "RuntimeRoot": "rroot"}.get(purpose, purpose)
        parts.append(f"v{v}@{loc}{('/' + tag) if tag else ''}")
    print(f"  i{i} {name} " + " ".join(parts))
    if ("After", i) in edits:
        print("    after:", ", ".join(edits[("After", i)]))
