#!/usr/bin/env python3
"""Line up two js_fn_profile.py JSON dumps (otter, node) by source line
(within 3 lines) and print self / inclusive ms per function and the ratio,
hottest otter-inclusive first.

usage: js_fn_compare.py <otter.json> <node.json> [top]
"""
import json, sys

o, n = json.load(open(sys.argv[1])), json.load(open(sys.argv[2]))
top = int(sys.argv[3]) if len(sys.argv) > 3 else 30
def line(k):
    return int(k.rsplit(":", 1)[1])
node_by_line = {}
for k in n["incl"]:
    node_by_line.setdefault(line(k), []).append(k)
def match(k):
    l = line(k)
    for d in (0, -1, 1, -2, 2, -3, 3):
        for cand in node_by_line.get(l + d, []):
            return cand
    return None
print(f"isolate ms: otter {o['total']} node {n['total']}  ratio {o['total'] / max(n['total'], 1):.2f}")
print(f"{'function':42} {'o.self':>7} {'n.self':>7} {'o.incl':>7} {'n.incl':>7} {'incl x':>6}")
rows = sorted(o["incl"].items(), key=lambda kv: -o["self"].get(kv[0], 0))[:top]
for k, inc in rows:
    m = match(k)
    ns = n["self"].get(m, 0) if m else 0
    ni = n["incl"].get(m, 0) if m else 0
    name = f"{k} ~ {m.split(':')[0]}" if m and k.startswith("<anon") else k
    ratio = f"{inc / ni:.2f}" if ni else "-"
    print(f"{name[:42]:42} {o['self'].get(k, 0):7d} {ns:7d} {inc:7d} {ni:7d} {ratio:>6}")
