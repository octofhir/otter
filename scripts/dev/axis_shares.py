#!/usr/bin/env python3
"""Inclusive main-thread share of each engine axis in macOS `sample` reports.

usage: axis_shares.py <sample.txt>...
Prints, per report, the share of otter-isolate samples whose stack holds a
frame of each axis: optimizing compilation (whole request and the graph
compiler alone), baseline compilation, minor and major collection, runtime
operations reached from generated code, and the interpreter.
"""
import re, sys

AXES = [
    ("opt-request", "compile_optimized_jit_function"),
    ("graph-compile", "5graph17compile_optimized"),
    ("template", "8template"),
    ("gc-minor", "collect_minor"),
    ("gc-major", "collect_full|collect_major|mark_phase|sweep_phase"),
    ("interp", "dispatch_ticks"),
    ("regexp", "regexp|otter_regex"),
]


def stacks(path):
    text = open(path).read()
    graph = text.split("Call graph:", 1)[1].split("Total number in stack", 1)[0]
    threads = re.split(r"\n    (?=\d+ Thread_)", graph)
    thread = next(t for t in threads if t.strip() and "otter-isolate" in t.strip().splitlines()[0])
    entries = []
    for line in thread.strip().splitlines():
        m = re.match(r"^([\s+!:|]*?)(\d+) (.*)$", line)
        if m:
            entries.append((len(m.group(1)), int(m.group(2)), m.group(3)))
    return entries


def shares(path):
    entries = stacks(path)
    total = entries[0][1]
    out = {}
    for name, pattern in AXES:
        rx = re.compile(pattern)
        count, inside = 0, []
        for depth, n, frame in entries:
            while inside and depth <= inside[-1]:
                inside.pop()
            if rx.search(frame):
                if not inside:
                    count += n
                inside.append(depth)
        out[name] = 100.0 * count / total
    return total, out


print(f"{'report':28} {'samples':>7} " + " ".join(f"{a:>13}" for a, _ in AXES))
for path in sys.argv[1:]:
    total, out = shares(path)
    print(f"{path.split('/')[-1]:28} {total:7d} " + " ".join(f"{out[a]:12.1f}%" for a, _ in AXES))
