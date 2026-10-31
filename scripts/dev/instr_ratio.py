#!/usr/bin/env python3
"""Per-target ratio and geomean of two web_tooling_instr_one.sh outputs.
usage: instr_ratio.py <numerator> <denominator>"""
import math, sys
a = dict(l.split() for l in open(sys.argv[1]) if l.strip())
b = dict(l.split() for l in open(sys.argv[2]) if l.strip())
rows = [(t, int(a[t]), int(b[t])) for t in a if t in b and a[t] and b[t]]
for t, x, y in sorted(rows, key=lambda r: -r[1] / r[2]):
    print(f"{t:14} {x/1e9:7.2f}G {y/1e9:7.2f}G {x/y:6.3f}")
print(f"geomean {math.exp(sum(math.log(x / y) for _, x, y in rows) / len(rows)):.3f}")
