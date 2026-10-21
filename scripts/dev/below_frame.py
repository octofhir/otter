#!/usr/bin/env python3
"""Samples under the innermost frame matching a name, by the frame below it.

usage: below_frame.py <sample.txt> <frame-substring> [top]
For every leaf sample of the `otter-isolate` thread whose stack holds a frame
containing the substring, credits the sample to the frame directly called by
the innermost such frame (`<leaf>` when it is the leaf itself). Shows where a
boundary routine's time actually goes when it recurses through itself.
"""
import collections, re, sys

text = open(sys.argv[1]).read()
needle = sys.argv[2]
top = int(sys.argv[3]) if len(sys.argv) > 3 else 30
graph = text.split("Call graph:", 1)[1].split("Total number in stack", 1)[0]
thread = next(t for t in re.split(r"\n    (?=\d+ Thread_)", graph) if "otter-isolate" in t.split("\n", 1)[0])
rows = []
for line in thread.splitlines():
    m = re.match(r"^([\s+!:|]*)(\d+) (.*)$", line)
    if m:
        rows.append((len(m.group(1)), int(m.group(2)), m.group(3).split("  (in")[0].split(" + ")[0]))
counts = collections.Counter()
stack = []
for i, (depth, count, name) in enumerate(rows):
    while stack and stack[-1][0] >= depth:
        stack.pop()
    stack.append((depth, name))
    children = sum(c for d, c, _ in rows[i + 1:i + 1 + 400] if d == (rows[i + 1][0] if i + 1 < len(rows) and rows[i + 1][0] > depth else -1))
    j, kids = i + 1, 0
    first = None
    while j < len(rows) and rows[j][0] > depth:
        if first is None:
            first = rows[j][0]
        if rows[j][0] == first:
            kids += rows[j][1]
        j += 1
    own = count - kids
    if own <= 0:
        continue
    idx = max((k for k, (_, n) in enumerate(stack) if needle in n), default=None)
    if idx is None:
        continue
    below = stack[idx + 1][1] if idx + 1 < len(stack) else "<leaf>"
    counts[below] += own
total = sum(counts.values())
print(f"{total} samples under {needle}")
for name, value in counts.most_common(top):
    print(f"{value:6d} {100 * value / total:5.1f}%  {name[:140]}")
