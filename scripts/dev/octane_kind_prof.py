#!/usr/bin/env python3
"""Self samples of optimized code per node kind, from an `octaneprof.py` run.

usage: octane_kind_prof.py <octaneprof-work-dir> [top]

Every self sample of the `otter-isolate` thread whose leaf address lies in
a code object of the run's artifacts is credited to the code-map region
holding it, summed by the region's operation kind (its name without the
payload). Template and other tiers sum under `<tier>`.
"""
import bisect, collections, json, pathlib, re, sys

work = pathlib.Path(sys.argv[1])
top = int(sys.argv[2]) if len(sys.argv) > 2 else 30
objects = []
for d in sorted((work / "art").glob("jit-*")):
    try:
        cm = json.loads((d / "code-map.json").read_text())
        man = json.loads((d / "manifest.json").read_text())
    except (OSError, json.JSONDecodeError):
        continue
    rng = cm.get("runtimeAddressRange") or {}
    if "start" not in rng:
        continue
    kinds = []
    if man["tier"] == "optimizing":
        for line in (d / "asm.txt").read_text().splitlines():
            m = re.search(r'range=\+0x([0-9a-f]+)\.\.\+0x([0-9a-f]+).*?tier-op="v\d+ ([A-Za-z0-9]+)', line)
            if m:
                kinds.append((int(m.group(1), 16), int(m.group(2), 16), m.group(3)))
    objects.append((int(rng["start"], 16), int(rng["endExclusive"], 16), man["tier"], sorted(kinds)))
objects.sort()
starts = [o[0] for o in objects]
text = (work / "sample.txt").read_text()
graph = text.split("Call graph:", 1)[1].split("Total number in stack", 1)[0]
thread = next(t for t in re.split(r"\n    (?=\d+ Thread_)", graph) if "otter-isolate" in t.split("\n", 1)[0])
rows = []
for line in thread.splitlines():
    m = re.match(r"^([\s+!:|]*)(\d+) (.*)$", line)
    if m:
        rows.append((len(m.group(1)), int(m.group(2)), m.group(3)))
total = rows[0][1]
counts = collections.Counter()
for i, (depth, count, frame) in enumerate(rows):
    j, kids, first = i + 1, 0, None
    while j < len(rows) and rows[j][0] > depth:
        if first is None:
            first = rows[j][0]
        if rows[j][0] == first:
            kids += rows[j][1]
        j += 1
    own = count - kids
    if own <= 0 or not frame.startswith("???"):
        continue
    addresses = [int(a, 16) for a in re.findall(r"0x([0-9a-f]+)", frame.split("[", 1)[1])]
    for address in addresses:
        k = bisect.bisect_right(starts, address) - 1
        share = own / len(addresses)
        if k < 0 or address >= objects[k][1]:
            counts["<unknown code>"] += share
            continue
        start, _, tier, kinds = objects[k]
        if tier != "optimizing":
            counts[f"<{tier}>"] += share
            continue
        offset = address - start
        hit = [kind for (s, e, kind) in kinds if s <= offset < e]
        counts[hit[-1] if hit else "<outside>"] += share
print(f"isolate samples {total}")
for name, value in counts.most_common(top):
    print(f"{100 * value / total:5.1f}% {name}")
