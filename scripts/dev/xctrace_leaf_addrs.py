#!/usr/bin/env python3
"""Leaf-instruction sample counts of one function from an xctrace Time
Profiler `time-profile` table export: which instructions of a hot Rust
function the samples land on.
usage: xctrace_leaf_addrs.py <time-profile.xml> <function-substring> [top]
Export the table first:
  xcrun xctrace export --input X.trace \
    --xpath '/trace-toc/run[@number="1"]/data/table[@schema="time-profile"]' > X.xml
"""
import collections
import sys
import xml.etree.ElementTree as ET

path, focus = sys.argv[1], sys.argv[2]
top = int(sys.argv[3]) if len(sys.argv) > 3 else 40
frames, backtraces = {}, {}
counts = collections.Counter()
for _, el in ET.iterparse(path, events=("end",)):
    if el.tag == "frame" and el.get("id"):
        frames[el.get("id")] = (el.get("name") or "", el.get("addr") or "")
    elif el.tag == "backtrace" and el.get("id"):
        first = el.find("frame")
        if first is not None:
            backtraces[el.get("id")] = first.get("id") or first.get("ref")
    elif el.tag == "row":
        bt = el.find(".//backtrace")
        if bt is None:
            continue
        key = bt.get("id") or bt.get("ref")
        frame = frames.get(backtraces.get(key))
        if frame and focus in frame[0]:
            counts[frame[1]] += 1
        el.clear()
total = sum(counts.values())
print(f"{total} leaf samples in functions matching {focus!r}")
for addr, n in sorted(counts.items(), key=lambda kv: int(kv[0], 16))[:10000]:
    if n * 200 >= total:
        print(f"  {addr}  {n:5d}  {100 * n / total:5.1f}%")
