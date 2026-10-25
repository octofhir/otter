#!/usr/bin/env python3
"""Inclusive isolate-thread samples per frame pattern from a macOS `sample`
report, each sample counted once per pattern (outermost matching frame).

usage: isolate_inclusive.py <sample.txt> <pattern> [pattern...]
"""
import re
import sys

text = open(sys.argv[1]).read()
patterns = sys.argv[2:]
graph = text.split("Call graph:")[1].split("Total number in stack")[0]
thread = next(t for t in re.split(r"\n    (?=\d+ Thread_)", graph)
              if "otter-isolate" in t.split("\n", 1)[0])
rows = []
for line in thread.splitlines():
    m = re.match(r"^([\s+!:|]*)(\d+) (.*)$", line)
    if m:
        rows.append((len(m.group(1)), int(m.group(2)), m.group(3)))
total = rows[0][1] if rows else 0
for pattern in patterns:
    count = 0
    inside = None
    for depth, n, frame in rows:
        if inside is not None and depth > inside:
            continue
        inside = None
        if pattern in frame:
            count += n
            inside = depth
    print(f"{100 * count / max(total, 1):5.1f}%  {count:6d}  {pattern}")
print(f"total {total}")
