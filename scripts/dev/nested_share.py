#!/usr/bin/env python3
"""Share of otter-isolate self samples whose stack holds a pattern at least
`k` times: `run_callable_sync_rooted 2` counts time inside JavaScript that a
native re-entered.

usage: nested_share.py <sample.txt> <pattern> [k]
"""
import re, sys

text = open(sys.argv[1]).read()
pat, k = sys.argv[2], int(sys.argv[3]) if len(sys.argv) > 3 else 2
graph = text.split("Call graph:", 1)[1].split("Total number in stack", 1)[0]
thread = graph.split("otter-isolate", 1)[1]
thread = re.split(r"\n    \d+ Thread_", thread, maxsplit=1)[0]
entries = []
for line in thread.splitlines():
    m = re.match(r"^([\s+!:|]*?)(\d+) (.*)$", line)
    if m:
        entries.append((len(m.group(1)), int(m.group(2)), m.group(3)))
stack, total, hit = [], 0, 0
for i, (depth, count, name) in enumerate(entries):
    while stack and stack[-1][0] >= depth:
        stack.pop()
    stack.append((depth, pat in name))
    children = 0
    for j in range(i + 1, len(entries)):
        if entries[j][0] <= depth:
            break
        if entries[j][0] == entries[i + 1][0] if i + 1 < len(entries) else False:
            children += entries[j][1]
    own = max(count - children, 0)
    total += own
    if sum(1 for _, h in stack if h) >= k:
        hit += own
print(f"{hit}/{total} = {100 * hit / max(total, 1):.1f}% with >= {k} x {pat}")
