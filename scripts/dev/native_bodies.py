#!/usr/bin/env python3
"""Isolate samples of each native body a `sample` capture entered, credited
to the first frame below the native-call machinery entered at
`otter_native_invoke`. A body's count excludes the bodies nested under
it, so an event native that runs JavaScript which calls more natives keeps
only its own share.

usage: native_bodies.py <sample.txt> [top]
"""
import collections, re, sys

text = open(sys.argv[1]).read()
top = int(sys.argv[2]) if len(sys.argv) > 2 else 30
graph = text.split("Call graph:", 1)[1].split("Total number in stack", 1)[0]
thread = graph.split("otter-isolate", 1)[1].split("\n    ", 1)[1]
thread = re.split(r"\n    \d+ Thread_", thread, maxsplit=1)[0]
entries = []
for line in thread.splitlines():
    m = re.match(r"^([\s+!:|]*?)(\d+) (.*)$", line)
    if m:
        name = re.sub(r"\s*\(in .*", "", m.group(3)).strip()
        entries.append((len(m.group(1)), int(m.group(2)), name))

MACHINERY = (
    "otter_native_", "host_call", "native_call", "invoke_frame_native_call",
    "with_frame_native_ctx", "NativeCallTarget", "with_turn", "call_trampoline",
    "run_vm_intrinsic", "with_native_context", "with_host_realm_global",
    "NativeCallInfo", "RuntimeCall", "committed_values",
)
# Body entries: index -> (name, count), with the index of the nearest
# enclosing body entry.
body_of = {}
enclosing = {}
stack = []
for index, (depth, count, name) in enumerate(entries):
    while stack and entries[stack[-1]][0] >= depth:
        stack.pop()
    # Inside the machinery of a native call: every frame from the nearest
    # `otter_native_invoke` down is machinery until the first body.
    entered = False
    for i in reversed(stack):
        frame = entries[i][2]
        if i in body_of:
            break
        if "otter_native_invoke" in frame:
            entered = True
            break
        if not any(part in frame for part in MACHINERY):
            break
    if entered and not any(part in name for part in MACHINERY):
        body_of[index] = (name, count)
        enclosing[index] = next((i for i in reversed(stack) if i in body_of), None)
    stack.append(index)

own = {index: count for index, (_, count) in body_of.items()}
for index, outer in enclosing.items():
    if outer is not None:
        own[outer] -= body_of[index][1]
bodies = collections.Counter()
for index, count in own.items():
    bodies[body_of[index][0]] += count

total = sum(count for depth, count, _ in entries if depth == min(e[0] for e in entries))
print(f"isolate samples {total}; native bodies {sum(bodies.values())}")
for name, count in bodies.most_common(top):
    short = re.sub(r"_RNv[A-Za-z0-9_]*?Cs[A-Za-z0-9]+_\d+", "", name)[:120]
    print(f"{count:6d} {100.0 * count / max(total, 1):5.1f}%  {short}")
