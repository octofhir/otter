#!/usr/bin/env python3
"""Inclusive share of runtime families on the otter-isolate thread of one
macOS `sample` report.

usage: sample_families.py <sample.txt> [pattern=label ...]

A family's share counts every sample whose stack holds a frame matching its
pattern, taken at the outermost such frame, so nested matches are not counted
twice. The default families split the isolate's time into compilation work on
the isolate, collection, interpreter dispatch, runtime stubs entered from
generated code and host calls; extra `regex=label` arguments add families.
"""
import re
import sys

DEFAULT = [
    (r"jit_compile|bake_|JitCodeRegistry|install_compiled|compile_queue", "compile on isolate"),
    (r"otter_gc", "gc"),
    (r"interp8dispatch|dispatch_ticks|run_frame|execute_instruction", "interpreter dispatch"),
    (r"jit_runtime|runtime_stubs|property_dispatch|property_cache|property_ic", "runtime stubs / property"),
    (r"host_call", "host calls"),
    (r"regexp|regex", "regexp"),
    (r"string", "strings"),
]

text = open(sys.argv[1]).read()
families = DEFAULT + [tuple(a.split("=", 1)) for a in sys.argv[2:]]
graph = text.split("Call graph:", 1)[1].split("Total number in stack", 1)[0]
thread = graph.split("otter-isolate", 1)[1]
thread = re.split(r"\n    \d+ Thread_", thread, maxsplit=1)[0]
entries = []
for line in thread.splitlines()[1:]:
    m = re.match(r"^([\s+!:|]*?)(\d+) (.*)$", line)
    if m:
        entries.append((len(m.group(1)), int(m.group(2)), m.group(3)))
total = entries[0][1] if entries else 0
print(f"isolate samples: {total}")
for pattern, label in families:
    rx = re.compile(pattern)
    inside = []  # depths of matching ancestors on the current path
    count = 0
    for depth, n, name in entries:
        while inside and inside[-1] >= depth:
            inside.pop()
        if rx.search(name):
            if not inside:
                count += n
            inside.append(depth)
    print(f"  {count:7d} {100.0 * count / max(total, 1):5.1f}%  {label}")
