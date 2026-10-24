#!/usr/bin/env python3
"""Unconditional branches in a JIT asm.txt whose target is itself an
unconditional branch, and branches to the next instruction.

usage: jump_chains.py <asm.txt>...
"""
import re
import sys

for path in sys.argv[1:]:
    insns = {}
    order = []
    for line in open(path):
        m = re.match(r"^\+0x([0-9a-f]+): [0-9a-f]{8}  (.*)$", line)
        if m:
            addr = int(m.group(1), 16)
            insns[addr] = m.group(2).strip()
            order.append(addr)
    branches = chains = to_next = 0
    for i, addr in enumerate(order):
        m = re.match(r"^b L([0-9a-f]+)$", insns[addr])
        if not m:
            continue
        branches += 1
        target = int(m.group(1), 16)
        if re.match(r"^b L[0-9a-f]+$", insns.get(target, "")):
            chains += 1
        if i + 1 < len(order) and order[i + 1] == target:
            to_next += 1
    print(f"{path}: {len(order)} insns, {branches} b, {chains} b->b, {to_next} b->next")
