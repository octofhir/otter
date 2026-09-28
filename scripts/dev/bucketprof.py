#!/usr/bin/env python3
"""Whole-run time buckets for one workload.

usage: bucketprof.py <sample.txt>
Folds the `otter-isolate` thread's self samples from a macOS `sample` report
into coarse buckets (generated code, optimizing compiler, baseline compiler,
GC, interpreter, runtime natives, allocator) so the largest lever of a
workload is visible at a glance.
"""
import collections
import re
import sys

text = open(sys.argv[1]).read()
graph = text.split("Call graph:", 1)[1].split("Total number in stack", 1)[0]
threads = re.split(r"\n    (?=\d+ Thread_)", graph)
BUCKETS = [
    ("jit-code", lambda s: s == "???" or "(in <unknown binary>)" in s),
    ("regalloc", lambda s: "regalloc2" in s),
    ("jit-compiler", lambda s: "otter_jit" in s and ("machine" in s or "optimizing" in s or "numeric" in s)),
    ("template-compiler", lambda s: "otter_jit" in s and "template" in s),
    ("jit-other", lambda s: "otter_jit" in s),
    ("gc", lambda s: "otter_gc" in s),
    ("interpreter", lambda s: "dispatch" in s or "interp" in s),
    ("compiler(bytecode)", lambda s: "otter_compiler" in s or "oxc" in s),
    ("vm-runtime", lambda s: "otter_vm" in s),
    ("malloc", lambda s: "malloc" in s or "free" in s or "xzm" in s or "memmove" in s or "memset" in s or "platform_" in s),
]
totals = collections.Counter()
for thread in threads:
    head = thread.split("\n", 1)[0]
    entries = []
    for line in thread.splitlines()[1:]:
        m = re.match(r"^([\s+!:|]*?)(\d+) (.*)$", line)
        if m:
            entries.append([len(m.group(1)), int(m.group(2)), m.group(3), int(m.group(2))])
    stack = []
    for entry in entries:
        while stack and stack[-1][0] >= entry[0]:
            stack.pop()
        if stack:
            stack[-1][3] -= entry[1]
        stack.append(entry)
    thread_kind = "isolate" if "otter-isolate" in head else "other-threads"
    for depth, count, name, self_count in entries:
        if self_count <= 0:
            continue
        bucket = next((b for b, test in BUCKETS if test(name)), "other")
        totals[(thread_kind, bucket)] += self_count
grand = sum(totals.values())
for (thread_kind, bucket), count in totals.most_common(24):
    print(f"{thread_kind:13} {bucket:20} {count:7} {100 * count / grand:5.1f}%")
