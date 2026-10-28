#!/usr/bin/env python3
"""Failing-set difference of two test262 result files: new and fixed failures.
usage: scripts/dev/t262_diff.py <before.json> <after.json>"""
import json, sys
def failing(path):
    tests = json.load(open(path))["tests"]
    items = tests.items() if isinstance(tests, dict) else ((t.get("path") or t.get("name"), t) for t in tests)
    out = set()
    for name, t in items:
        status = t if isinstance(t, str) else (t.get("outcome") or t.get("status") or t.get("result"))
        if isinstance(status, dict): status = status.get("kind")
        if str(status).lower() not in ("pass", "passed", "skip", "skipped"):
            out.add(name)
    return out
a, b = failing(sys.argv[1]), failing(sys.argv[2])
print(f"before {len(a)} after {len(b)}")
for n in sorted(b - a): print("NEW", n)
for n in sorted(a - b): print("FIXED", n)
