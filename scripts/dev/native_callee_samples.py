#!/usr/bin/env python3
"""Isolate-thread samples of a macOS `sample` capture inside native builtins,
credited to the builtin each native call entered (the first frame below the
host-call path), inclusive of its work.
usage: native_callee_samples.py <sample.txt> [top]
"""
import collections
import re
import sys

text = open(sys.argv[1]).read()
top = int(sys.argv[2]) if len(sys.argv) > 2 else 30
graph = text.split("Call graph:", 1)[1].split("Total number in stack", 1)[0]
thread = next(t for t in re.split(r"\n    (?=\d+ Thread_)", graph)
              if "otter-isolate" in t.split("\n", 1)[0])
line_re = re.compile(r"^(\s*)[+!:| ]*(\d+) (.*?)(?:  \(in [^)]*\))?(?: \+ \d+.*)?$")
# Frames of the host-call path between generated code and the builtin.
WRAPPER = re.compile(r"host_call_entry|host_call11native_call|invoke_frame_native_call|"
                     r"with_frame_native_ctx|NativeCallTarget|native_calls|zero_sized_entry|"
                     r"otter_native_invoke|run_vm_intrinsic|with_turn|with_host_realm")
stack = []  # (indent, name)
counts = collections.Counter()
for line in thread.splitlines()[1:]:
    m = line_re.match(line)
    if not m:
        continue
    indent = len(line) - len(line.lstrip(" +!:|"))
    n, name = int(m.group(2)), m.group(3).strip()
    while stack and stack[-1][0] >= indent:
        stack.pop()
    if stack and WRAPPER.search(stack[-1][1]) and not WRAPPER.search(name):
        counts[re.sub(r"_RN[a-zA-Z0-9]*Cs[0-9a-zA-Z]*_[0-9]*", "", name)[:110]] += n
    stack.append((indent, name))
total = sum(counts.values())
print("samples inside invoked natives", total)
for name, n in counts.most_common(top):
    print(f"{n:6d}  {name}")
