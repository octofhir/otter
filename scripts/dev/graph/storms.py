#!/usr/bin/env python3
"""Recompile storms of the optimizing tier from one --jit-events run.

usage: storms.py <otter> <out.json> <script> [script args...]
Prints the functions compiled most often and, for each, the bail sites
(resume PC, exit reason, action, opcode) that kept retiring them.
"""
import collections, json, subprocess, sys

otter, out, *script = sys.argv[1:]
subprocess.run([otter, f"--jit-events={out}", "run", *script],
               stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=1800)
events = json.load(open(out))["events"]
names = {e["functionId"]: e.get("functionName") for e in events if "functionName" in e}
compiles = collections.Counter()
for e in events:
    if e.get("type") == "compileFinished" and e.get("tier") == "optimizing":
        compiles[e["functionId"]] += 1
bails = collections.defaultdict(collections.Counter)
for e in events:
    if e.get("type") == "bail" and e.get("tier") == "optimizing":
        key = (e.get("resumePc"), e.get("exitReason"), e.get("exitAction"), e.get("opDebug"))
        bails[e.get("functionId")][key] += 1
entered = collections.defaultdict(collections.Counter)
for e in events:
    if e.get("type") == "enteredGenerationDeopt":
        key = (e.get("calleeResumePc"), e.get("exitReason"), e.get("exitAction"))
        entered[e.get("calleeFunctionId")][key] += 1
for fid, count in compiles.most_common(8):
    print(f"fid {fid} {names.get(fid)!r}: {count} optimizing compiles")
    for key, n in bails[fid].most_common(4):
        print(f"    bail {n}x {key}")
    for key, n in entered[fid].most_common(3):
        print(f"    entered-deopt {n}x {key}")
