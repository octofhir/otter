#!/usr/bin/env python3
"""Samples of the otter isolate thread with no JS code object on the stack,
grouped by the outermost otter frame below the thread entry that names a
phase (compile, parse, GC, startup, interpreter) and by leaf function.

usage: no_js_frames.py <sample.txt> <perf.map> [top]
"""
import bisect, collections, re, sys

sample, perf_map = sys.argv[1:3]
top = int(sys.argv[3]) if len(sys.argv) > 3 else 30
text = open(sample).read()
graph = text.split("Call graph:", 1)[1].split("Total number in stack", 1)[0]
thread = graph.split("otter-isolate", 1)[1].split("\n", 1)[1]
thread = re.split(r"\n    \d+ Thread_", thread, maxsplit=1)[0]
entries = []
for line in thread.splitlines():
    m = re.match(r"^([\s+!:|]*?)(\d+) (.*)$", line)
    if m:
        entries.append((len(m.group(1)), int(m.group(2)), m.group(3)))
codes = sorted((int(a, 16), int(b, 16)) for a, b, _ in (l.split(" ", 2) for l in open(perf_map)))
starts = [c[0] for c in codes]
def in_code(name):
    m = re.search(r"\?\?\?.*\[0x([0-9a-f]+)\]", name)
    if not m:
        return False
    pc = int(m.group(1), 16)
    i = bisect.bisect_right(starts, pc) - 1
    return i >= 0 and pc < codes[i][0] + codes[i][1]
def short(name):
    name = re.sub(r"\s+\(in [^)]*\).*$", "", name)
    name = re.sub(r"::h[0-9a-f]{16}$", "", name)
    return name[:110]
PHASE = re.compile(r"(compile|parse|Parser|codegen|gc_|collect|scaveng|mark|sweep|bootstrap|startup|snapshot|interp(ret)?|run_loop|dispatch|module|link|evaluate|microtask|lower|bake)", re.I)
leaf_c, phase_c = collections.Counter(), collections.Counter()
stack = []
total = 0
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
    if own <= 0 or any(in_code(n) for _, n in stack):
        continue
    total += own
    leaf_c[short(name)] += own
    # deepest frame matching a phase word, from the top of the stack down
    names = [short(n) for _, n in stack]
    ph = next((n for n in reversed(names) if PHASE.search(n) and "otter" in n), names[-1])
    phase_c[ph] += own
print(f"no-js samples: {total}")
print("== by deepest phase frame")
for k, v in phase_c.most_common(top):
    print(f"{v:6d} {100.0 * v / max(total, 1):5.1f}%  {k}")
print("== by leaf")
for k, v in leaf_c.most_common(top):
    print(f"{v:6d} {100.0 * v / max(total, 1):5.1f}%  {k}")
