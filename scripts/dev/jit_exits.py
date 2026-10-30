#!/usr/bin/env python3
"""otter-isolate self samples outside generated code, credited to the first
Rust frame that generated code called: which runtime entries the compiled
program spends its non-generated time in.

usage: jit_exits.py <sample.txt> [top] [entry-substring] [frames]
With an entry substring, ranks instead the `frames` (default 3) named frames
below that entry: what the entry spends its own runtime time in.
"""
import collections, re, sys

text = open(sys.argv[1]).read()
top = int(sys.argv[2]) if len(sys.argv) > 2 else 30
focus = sys.argv[3] if len(sys.argv) > 3 else None
frames = int(sys.argv[4]) if len(sys.argv) > 4 else 3
graph = text.split("Call graph:", 1)[1].split("Total number in stack", 1)[0]
thread = graph.split("otter-isolate", 1)[1]
thread = re.split(r"\n    \d+ Thread_", thread, maxsplit=1)[0]
entries = []
for line in thread.splitlines():
    m = re.match(r"^([\s+!:|]*?)(\d+) (.*)$", line)
    if m:
        name = re.sub(r"\s*\(in .*", "", m.group(3)).strip()
        entries.append((len(m.group(1)), int(m.group(2)), name))
def short(n):
    parts = re.findall(r"\d+([A-Za-z_][A-Za-z0-9_]*)", n)
    parts = [p for p in parts if not re.match(r"^(Cs|Nt|Nv|Ms|B\d|E$)", p)]
    return "::".join(parts[-2:]) if parts else n[:80]
cats = collections.Counter()
stack, total = [], 0
for i, (depth, count, name) in enumerate(entries):
    while stack and stack[-1][0] >= depth:
        stack.pop()
    stack.append((depth, name))
    children = 0
    if i + 1 < len(entries) and entries[i + 1][0] > depth:
        first = entries[i + 1][0]
        for j in range(i + 1, len(entries)):
            if entries[j][0] <= depth:
                break
            if entries[j][0] == first:
                children += entries[j][1]
    own = max(count - children, 0)
    if not own:
        continue
    total += own
    if name.startswith("???"):
        cats["(generated)"] += own
        continue
    entry = None
    for k in range(len(stack) - 1, 0, -1):
        if stack[k - 1][1].startswith("???") and not stack[k][1].startswith("???"):
            entry = k
            break
    if focus is not None:
        if entry is not None and focus in stack[entry][1]:
            path = [short(n) for _, n in stack[entry + 1 : entry + 1 + frames]]
            cats[" > ".join(path) or "(self)"] += own
        continue
    cats[short(stack[entry][1]) if entry is not None else "(not under generated code)"] += own
for name, n in cats.most_common(top):
    print(f"{n:6d} {100 * n / total:5.1f}%  {name}")
