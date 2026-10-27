#!/usr/bin/env python3
"""Self samples of the otter-isolate thread in coarse categories: generated
code, the bytecode interpreter, garbage collection, JIT compilation on the
isolate, bytecode compilation and the rest.

usage: isolate_categories.py <sample.txt>
"""
import collections, re, sys

text = open(sys.argv[1]).read()
graph = text.split("Call graph:", 1)[1].split("Total number in stack", 1)[0]
thread = graph.split("otter-isolate", 1)[1]
thread = re.split(r"\n    \d+ Thread_", thread, maxsplit=1)[0]
entries = []
for line in thread.splitlines():
    m = re.match(r"^([\s+!:|]*?)(\d+) (.*)$", line)
    if m:
        entries.append((len(m.group(1)), int(m.group(2)), re.sub(r"\s*\(in .*", "", m.group(3)).strip()))
cats = collections.Counter()
stack = []
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
    names = " ".join(n for _, n in stack)
    if re.search(r"cvwait|kevent|psynch", name):
        key = "waiting"
    elif "otter_gc" in names:
        key = "gc"
    elif "otter_jit" in names and re.search(r"compile|lower|regalloc|builder|Codegen", names):
        key = "jit compile on isolate"
    elif "otter_compiler" in names or "oxc_parser" in names:
        key = "bytecode compile/parse"
    elif "???" in name:
        key = "generated (+routines)"
    elif re.search(r"interp\d*dispatch|run_dispatch|execute_activation|interp4exec", names) and "???" not in names:
        key = "interpreter"
    else:
        key = "runtime/other"
    cats[key] += own
total = sum(cats.values())
for key, count in cats.most_common():
    print(f"{count:6d} {100 * count / max(total, 1):5.1f}%  {key}")
