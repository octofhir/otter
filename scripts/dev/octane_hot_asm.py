#!/usr/bin/env python3
"""Hot instructions of one compiled function from an `octaneprof.py` run.

usage: octane_hot_asm.py <octaneprof-work-dir> <function#id/tier> [min-samples]

Credits every self sample of the `otter-isolate` thread whose leaf address
lies in the code object labelled `<name>#<function id>/<tier initial>` to
the instruction holding it, then prints that object's listing: per-region
headers (the node or operation) with their sample totals and every
instruction at or above `min-samples`, plus the region totals ranked.
"""
import collections, json, pathlib, re, sys

work = pathlib.Path(sys.argv[1])
want = sys.argv[2]
minimum = float(sys.argv[3]) if len(sys.argv) > 3 else 2
target = None
for d in sorted((work / "art").glob("jit-*")):
    try:
        cm = json.loads((d / "code-map.json").read_text())
        man = json.loads((d / "manifest.json").read_text())
    except (OSError, json.JSONDecodeError):
        continue
    rng = cm.get("runtimeAddressRange") or {}
    label = f'{man["functionName"]}#{man.get("functionId", "?")}/{man["tier"][0]}'
    if label == want and "start" in rng:
        target = (int(rng["start"], 16), int(rng["endExclusive"], 16), d)
if not target:
    sys.exit(f"no code object {want}")
start, end, art = target
text = (work / "sample.txt").read_text()
graph = text.split("Call graph:", 1)[1].split("Total number in stack", 1)[0]
thread = next(t for t in re.split(r"\n    (?=\d+ Thread_)", graph) if "otter-isolate" in t.split("\n", 1)[0])
rows = []
for line in thread.splitlines():
    m = re.match(r"^([\s+!:|]*)(\d+) (.*)$", line)
    if m:
        rows.append((len(m.group(1)), int(m.group(2)), m.group(3)))
hits = collections.Counter()
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
        if start <= address < end:
            hits[(address - start) & ~3] += own / len(addresses)
total = sum(hits.values())
print(f"{want}: {total:.0f} self samples")
regions = collections.Counter()
current = "(prologue)"
for line in (art / "asm.txt").read_text().splitlines():
    m = re.search(r'tier-op="([^"]*)"', line)
    if m:
        current = m.group(1)[:110]
        continue
    m = re.match(r"\+0x([0-9a-f]+): (.*)$", line)
    if m:
        offset = int(m.group(1), 16)
        regions[current] += hits.get(offset, 0)
        if hits.get(offset, 0) >= minimum:
            print(f"{hits[offset]:6.0f}  +0x{offset:05x} {m.group(2)[:70]}   ; {current[:60]}")
print("\n== regions")
for name, value in regions.most_common(40):
    if value:
        print(f"{value:6.0f} {100 * value / total:5.1f}%  {name}")
