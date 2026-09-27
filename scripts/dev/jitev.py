#!/usr/bin/env python3
"""Summarize an otter --jit-events=path dump.

Prints event counts, generated-call deopts by (callee, pc, reason, action),
and finished compiles by (function, tier).
Usage: jitev.py events.json [top_n]
"""
import collections, json, sys

d = json.load(open(sys.argv[1]))
top = int(sys.argv[2]) if len(sys.argv) > 2 else 15
ev = d["events"]
print("events:", dict(collections.Counter(e["type"] for e in ev)), "dropped:", d.get("droppedEvents"))
deopts = collections.Counter(
    (e.get("calleeFunctionId"), e.get("calleeResumePc"), e.get("exitReason"), e.get("exitAction"))
    for e in ev
    if e["type"] == "generatedCallDeopt"
)
print("-- generatedCallDeopt (calleeFid, resumePc, reason, action)")
for k, v in deopts.most_common(top):
    print(v, *k)
compiles = collections.Counter(
    (e.get("functionId"), e.get("tier")) for e in ev if e["type"] == "compileFinished"
)
print("-- compileFinished (fid, tier)")
for k, v in compiles.most_common(top):
    print(v, *k)
