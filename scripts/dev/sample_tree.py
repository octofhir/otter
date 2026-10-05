#!/usr/bin/env python3
"""Print the JS isolate thread of a macOS `sample` report as a pruned call tree.

Usage: scripts/dev/sample_tree.py <sample.txt> [min_count] [thread-substring]
"""
import re
import sys

path = sys.argv[1]
minimum = int(sys.argv[2]) if len(sys.argv) > 2 else 20
needle = sys.argv[3] if len(sys.argv) > 3 else "otter-isolate"
text = open(path).read()
graph = text.split("Call graph:")[1].split("Total number in stack")[0]
for thread in re.split(r"\n    (?=\d+ Thread_)", graph):
    if needle not in thread.split("\n", 1)[0]:
        continue
    for line in thread.split("\n"):
        m = re.match(r"^([\s+!:|]*)(\d+) (.*)$", line)
        if not m:
            continue
        depth, count, name = len(m.group(1)), int(m.group(2)), m.group(3)
        name = re.sub(r"\s+\(in otter\).*", "", name)
        name = re.sub(r"\[0x[0-9a-f]+\]", "", name)
        if count >= minimum:
            print(f"{depth:3d} {count:6d} {name[:160]}")
