#!/usr/bin/env python3
"""Isolate-thread samples attributed to the runtime entry generated code called.

usage: stubprof.py <sample.txt> [top]
Walks every leaf stack of the `otter-isolate` thread in a macOS `sample`
report. A self sample in generated code (unsymbolized frames) counts as
`<jit>`; any other sample is charged to the innermost named frame entered
directly from generated code (the stub boundary), or to `<no-jit>` when the
stack never passed through generated code. The rows answer "which runtime
entry costs how much, nested runtime work included".
"""
import collections, re, sys

text = open(sys.argv[1]).read()
top = int(sys.argv[2]) if len(sys.argv) > 2 else 30
graph = text.split("Call graph:", 1)[1].split("Total number in stack", 1)[0]
thread = next(t for t in re.split(r"\n    (?=\d+ Thread_)", graph)
              if "otter-isolate" in t.split("\n", 1)[0])
rows = []
for line in thread.splitlines():
    m = re.match(r"^([\s+!:|]*)(\d+) (.*)$", line)
    if m:
        name = re.sub(r"\s+\(in [^)]*\).*", "", m.group(3)).strip()
        rows.append((len(m.group(1)), int(m.group(2)), name))


def short(name):
    parts = re.findall(r"\d+([A-Za-z_][A-Za-z0-9_]*)", name)
    parts = [p for p in parts if not re.fullmatch(r"[A-Z][a-z]?[0-9a-zA-Z]{0,3}", p)]
    return "::".join(parts[-2:]) if parts else name[:70]


charged = collections.Counter()
stack = []
total = 0
for i, (depth, count, name) in enumerate(rows):
    while stack and stack[-1][0] >= depth:
        stack.pop()
    stack.append((depth, name))
    # Self samples: inclusive count minus direct children.
    children = 0
    j = i + 1
    child_depth = None
    while j < len(rows) and rows[j][0] > depth:
        if child_depth is None:
            child_depth = rows[j][0]
        if rows[j][0] == child_depth:
            children += rows[j][1]
        j += 1
    own = count - children
    if own <= 0:
        continue
    total += own
    names = [n for _, n in stack]
    if names[-1] == "???":
        charged["<jit>"] += own
        continue
    entry = None
    for k in range(len(names) - 1, 0, -1):
        if names[k - 1] == "???" and names[k] != "???":
            entry = names[k]
            break
    charged[short(entry) if entry else "<no-jit>"] += own
print(f"isolate self samples: {total}")
for name, count in charged.most_common(top):
    print(f"{count:7d} {100.0 * count / total:5.1f}%  {name}")
