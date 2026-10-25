#!/usr/bin/env python3
"""Generated-code bytes per region kind across a `--jit-artifacts` run:
graph node kinds for optimized code, operation names for template code, and
the non-instruction regions (prologues, epilogues, probes, literal pools).

usage: code_bytes_by_kind.py <artifact-root> [top]
"""
import collections, glob, json, os, re, sys

root = sys.argv[1]
top = int(sys.argv[2]) if len(sys.argv) > 2 else 40
by = {"optimizing": collections.Counter(), "template": collections.Counter()}
count = {"optimizing": collections.Counter(), "template": collections.Counter()}
total = collections.Counter()
for d in glob.glob(os.path.join(root, "jit-*")):
    tier = "optimizing" if "-optimizing-" in d else "template"
    try:
        cmap = json.load(open(os.path.join(d, "code-map.json")))
    except FileNotFoundError:
        continue
    size = int(re.search(r"code-bytes=(\d+)", open(os.path.join(d, "asm.txt")).read(4096)).group(1))
    total[tier] += size
    covered = 0
    for r in cmap["regions"]:
        n = r["endOffset"] - r["startOffset"]
        if r["kind"] == "instruction":
            op = r.get("operation", "")
            name = re.sub(r"^v\d+ ", "", op)
            name = re.split(r"[ ({\[]", name, 1)[0]
        else:
            name = "<" + r["kind"] + ">"
        by[tier][name] += n
        count[tier][name] += 1
        covered += n
    by[tier]["<uncovered>"] += max(0, size - covered)
for tier in ("optimizing", "template"):
    print(f"{tier}: {total[tier]/1e6:.2f} MB")
    for name, n in by[tier].most_common(top):
        c = count[tier][name]
        print(f"  {n/1e6:7.2f} MB {100*n/max(1,total[tier]):5.1f}%  {c:7} x {n/max(1,c):7.0f} B  {name}")
