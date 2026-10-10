#!/usr/bin/env python3
"""Inclusive samples per function on the otter-isolate thread of a `sample` report.

usage: inclprof.py <sample.txt> [top] [substring]
A frame counts once per stack even when it recurses, so a function's number is
the share of samples it was anywhere on the stack. An optional substring keeps
only matching names (e.g. `otter_vm` or `regexp`).
"""
import collections, re, sys

text = open(sys.argv[1]).read()
top = int(sys.argv[2]) if len(sys.argv) > 2 else 60
needle = sys.argv[3] if len(sys.argv) > 3 else None
graph = text.split("Call graph:", 1)[1].split("Total number in stack", 1)[0]
threads = re.split(r"\n    (?=\d+ Thread_)", graph)
thread = next(t for t in threads if t.strip() and "otter-isolate" in t.strip().splitlines()[0])
thread = thread.strip()
total = int(re.match(r"(\d+)", thread).group(1))


def short(name):
    name = re.sub(r"\s+\(in .*$", "", name)
    name = re.sub(r"\s+\+ \d+.*$", "", name)
    parts = re.findall(r"\d+([A-Za-z_][A-Za-z0-9_]*)", name.split("Cs", 1)[-1])
    if name.startswith("_R") and parts:
        return "::".join(p for p in parts if not re.fullmatch(r"B\d*_?|Nt|Ns|Nv|M[a-z0-9]*_?", p))[:110]
    return name[:110]


incl = collections.Counter()
stack = []  # (depth, name)
for line in thread.splitlines()[1:]:
    m = re.match(r"^([\s+!:|]*?)(\d+) (.*)$", line)
    if not m:
        continue
    depth, count, name = len(m.group(1)), int(m.group(2)), short(m.group(3))
    while stack and stack[-1][0] >= depth:
        stack.pop()
    if all(n != name for _, n in stack):
        incl[name] += count
    stack.append((depth, name))

for name, count in incl.most_common():
    if needle and needle not in name:
        continue
    print(f"{count:7d} {100.0 * count / total:5.1f}%  {name}")
    top -= 1
    if top == 0:
        break
