#!/usr/bin/env python3
"""Self samples per function with their hottest caller chains, from a macOS
`sample` report, for the JS isolate thread.

Usage: scripts/dev/sample_callers.py <sample.txt> [top] [chain-depth] [thread-substring]
"""
import collections
import re
import sys

path = sys.argv[1]
top = int(sys.argv[2]) if len(sys.argv) > 2 else 12
depth = int(sys.argv[3]) if len(sys.argv) > 3 else 5
needle = sys.argv[4] if len(sys.argv) > 4 else "otter-isolate"
text = open(path).read()
graph = text.split("Call graph:")[1].split("Total number in stack")[0]
thread = next(t for t in re.split(r"\n    (?=\d+ Thread_)", graph)
              if needle in t.split("\n", 1)[0])


def short(name):
    parts = re.findall(r"\d+([A-Za-z_][A-Za-z0-9_]*)", name)
    parts = [p for p in parts if not re.fullmatch(r"[A-Z][a-z]?[0-9a-zA-Z]{0,3}", p)]
    return "::".join(parts[-2:]) if parts else name[:70]


rows = []
for line in thread.split("\n"):
    m = re.match(r"^([\s+!:|]*)(\d+) (.*)$", line)
    if m:
        name = re.sub(r"\s+\(in [^)]*\).*", "", m.group(3)).strip()
        rows.append((len(m.group(1)), int(m.group(2)), name))
selfc = collections.Counter()
chains = collections.defaultdict(collections.Counter)
stack = []
for i, (d, n, name) in enumerate(rows):
    while stack and stack[-1][0] >= d:
        stack.pop()
    stack.append((d, n, name))
    child_depth, child_sum, j = None, 0, i + 1
    while j < len(rows) and rows[j][0] > d:
        if child_depth is None:
            child_depth = rows[j][0]
        if rows[j][0] == child_depth:
            child_sum += rows[j][1]
        j += 1
    own = n - child_sum
    if own > 0:
        selfc[name] += own
        chain = [short(x[2]) for x in reversed(stack[-depth - 1:-1])]
        chains[name][" <- ".join(chain)] += own
total = sum(selfc.values())
print(f"self samples: {total}")
for name, count in selfc.most_common(top):
    print(f"{count:6d} {100 * count / total:5.1f}% {short(name)}")
    for chain, c in chains[name].most_common(3):
        print(f"           {c:6d} <- {chain}")
