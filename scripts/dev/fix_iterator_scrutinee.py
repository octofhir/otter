#!/usr/bin/env python3
"""Rewrite `match state` / `= state {` scrutinees over an iterator body to
the wrapped `IteratorState` (`&**state` under read_payload, `&mut **state`
under with_payload), for the pattern errors cargo reports.

usage: cargo check --message-format=short 2>&1 | fix_iterator_scrutinee.py
"""
import collections
import re
import sys

done = collections.defaultdict(set)
for line in sys.stdin:
    m = re.match(r"(crates/[^:]+):(\d+):\d+: error\[E0308\]: mismatched types: expected `IteratorBody`, found `IteratorState`", line)
    if not m:
        continue
    path, row = m.group(1), int(m.group(2)) - 1
    lines = open(path).read().split("\n")
    k = row
    while k >= 0 and not re.search(r"\bmatch state\b|= state\s*\{|= state$", lines[k]):
        k -= 1
    if k < 0 or k in done[path]:
        continue
    j = k
    mutable = None
    while j >= 0:
        if "with_payload" in lines[j]:
            mutable = True
            break
        if "read_payload" in lines[j]:
            mutable = False
            break
        j -= 1
    target = "&mut **state" if mutable else "&**state"
    lines[k] = re.sub(r"\bmatch state\b", f"match {target}", lines[k])
    lines[k] = re.sub(r"= state(\s*\{|$)", lambda mm: f"= {target}{mm.group(1)}", lines[k])
    done[path].add(k)
    open(path, "w").write("\n".join(lines))
for path, rows in done.items():
    print(path, len(rows))
