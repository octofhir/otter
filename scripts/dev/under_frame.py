#!/usr/bin/env python3
"""Self samples of the otter isolate thread under the innermost frame
matching a substring with no generated-code frame below it, by leaf and by
the frame directly below the match.

usage: under_frame.py <sample.txt> <perf.map> <substring> [top]
"""
import bisect, collections, re, sys

sample, perf_map, needle = sys.argv[1:4]
top = int(sys.argv[4]) if len(sys.argv) > 4 else 20
text = open(sample).read()
graph = text.split("Call graph:", 1)[1].split("Total number in stack", 1)[0]
thread = graph.split("otter-isolate", 1)[1].split("\n", 1)[1]
thread = re.split(r"\n    \d+ Thread_", thread, maxsplit=1)[0]
codes = sorted((int(a, 16), int(b, 16)) for a, b, _ in (l.split(" ", 2) for l in open(perf_map)))
starts = [c[0] for c in codes]
def generated(name):
    m = re.search(r"\?\?\?.*\[0x([0-9a-f]+)\]", name)
    if not m:
        return False
    pc = int(m.group(1), 16)
    i = bisect.bisect_right(starts, pc) - 1
    return i >= 0 and pc < codes[i][0] + codes[i][1]
def short(name):
    name = re.sub(r"\s+\(in [^)]*\).*$", "", name).strip()
    if name.startswith("_R"):
        words = [w for n, w in re.findall(r"(\d+)([A-Za-z_][A-Za-z0-9_]*)", name)
                 if len(w) == int(n) and not re.fullmatch(r"Cs[0-9A-Za-z]+|h[0-9a-f]+", w)]
        name = "::".join(words[-3:]) or name
    return name[:100]
entries = []
for line in thread.splitlines():
    m = re.match(r"^([\s+!:|]*?)(\d+) (.*)$", line)
    if m:
        entries.append((len(m.group(1)), int(m.group(2)), m.group(3)))
leaf, below = collections.Counter(), collections.Counter()
stack, total = [], 0
for i, (depth, count, name) in enumerate(entries):
    while stack and stack[-1][0] >= depth:
        stack.pop()
    stack.append((depth, name))
    children, j = 0, i + 1
    while j < len(entries) and entries[j][0] > depth:
        if entries[j][0] == entries[i + 1][0]:
            children += entries[j][1]
        j += 1
    own = count - children
    if own <= 0:
        continue
    names = [n for _, n in stack]
    hits = [k for k, n in enumerate(names) if needle in n]
    if not hits:
        continue
    k = hits[-1]
    if any(generated(n) for n in names[k + 1:]):
        continue
    total += own
    leaf[short(name)] += own
    below[short(names[k + 1]) if k + 1 < len(names) else "(self)"] += own
print(f"samples under {needle}: {total}")
print("== frame below")
for key, v in below.most_common(top):
    print(f"{v:6d} {100.0 * v / max(total, 1):5.1f}%  {key}")
print("== leaf")
for key, v in leaf.most_common(top):
    print(f"{v:6d} {100.0 * v / max(total, 1):5.1f}%  {key}")
