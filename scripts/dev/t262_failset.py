#!/usr/bin/env python3
"""Failing test paths of a merged test262 JSON, one per line, sorted.

usage: t262_failset.py <latest.json> [base-failset.txt]
With a base list, prints NEW failures (absent from base) and FIXED ones,
and exits 1 when anything new failed.
"""
import json
import sys

data = json.load(open(sys.argv[1]))
bad = sorted(t["path"] for t in data["tests"]
             if t["outcome"].get("kind") not in ("pass", "skip", "skipped"))
if len(sys.argv) < 3:
    print("\n".join(bad))
    sys.exit(0)
base = set(open(sys.argv[2]).read().split())
new = [p for p in bad if p not in base]
fixed = sorted(base - set(bad))
print(f"failing {len(bad)} (base {len(base)}), new {len(new)}, fixed {len(fixed)}")
for p in new:
    print("NEW  ", p)
for p in fixed:
    print("FIXED", p)
sys.exit(1 if new else 0)
