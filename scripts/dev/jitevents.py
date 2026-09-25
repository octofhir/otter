#!/usr/bin/env python3
"""Summarize an `otter --jit-events=<file>` report for tier triage.

Usage: scripts/dev/jitevents.py <events.json> [top]

Prints compile outcomes per tier with decline reasons, optimized/template
bails by (function, reason, op), generated-call deopts, directCallPlan and
inline rejections by reason, and runtime property-store paths.
"""
import collections
import json
import re
import sys

path = sys.argv[1]
top = int(sys.argv[2]) if len(sys.argv) > 2 else 12
report = json.load(open(path))
events = report["events"]
names = {}
for e in events:
    if "functionName" in e and "functionId" in e:
        names[e["functionId"]] = e["functionName"]


def fname(fid):
    return f"{fid}:{names.get(fid, '?')[:40]}"


print(f"events={len(events)} truncated={report.get('truncated')}")
types = collections.Counter(e["type"] for e in events)
print("types:", dict(types.most_common()))

print("\n== compile outcomes")
outcomes = collections.Counter()
declines = collections.Counter()
for e in events:
    if e["type"] != "compileFinished":
        continue
    o = e["outcome"]
    outcomes[(e["tier"], o["kind"])] += 1
    if o["kind"] != "compiled":
        reason = re.sub(r"function \d+ ", "", str(o.get("reason", o.get("message", ""))))
        declines[(e["tier"], reason[:110])] += 1
for k, v in sorted(outcomes.items()):
    print(f"  {v:6d} {k}")
for (tier, reason), v in declines.most_common(top):
    print(f"  {v:6d} {tier} {reason}")

print("\n== bails (fn, tier, reason, op)")
bails = collections.Counter()
for e in events:
    if e["type"] == "bail":
        bails[(fname(e["functionId"]), e["tier"], e["exitReason"], e.get("opDebug"))] += 1
print(f"  total {sum(bails.values())}")
for k, v in bails.most_common(top):
    print(f"  {v:6d} {k}")
by_reason = collections.Counter()
for (f, t, r, op), v in bails.items():
    by_reason[(t, r, op)] += v
for k, v in by_reason.most_common(top):
    print(f"  {v:6d} {k}")

print("\n== generated call deopts (callee, reason)")
gd = collections.Counter()
for e in events:
    if e["type"] == "generatedCallDeopt":
        gd[(fname(e["calleeFunctionId"]), e["calleeTier"], e["exitReason"])] += 1
print(f"  total {sum(gd.values())}")
for k, v in gd.most_common(top):
    print(f"  {v:6d} {k}")

print("\n== directCallPlan outcomes")
dc = collections.Counter()
for e in events:
    if e["type"] == "directCallPlan":
        o = e["outcome"]
        key = o["kind"] if o["kind"] != "rejected" else "rejected:" + json.dumps(o.get("reason"))[:90]
        dc[(e["tier"], e["callKind"], key)] += 1
for k, v in dc.most_common(top):
    print(f"  {v:6d} {k}")

print("\n== inline candidates")
ic = collections.Counter()
for e in events:
    if e["type"] == "inlineCandidate":
        ic[json.dumps(e.get("bakeRejection", "accepted"))[:90]] += 1
for k, v in ic.most_common(top):
    print(f"  {v:6d} {k}")

print("\n== property store runtime paths")
ps = collections.Counter()
for e in events:
    if e["type"] == "propertyStoreRuntime":
        ps[(e["path"], e.get("propertyName"))] += e.get("count", 1)
for k, v in ps.most_common(top):
    print(f"  {v:6d} {k}")
