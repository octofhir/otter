#!/usr/bin/env python3
"""Per-region JIT profile.

usage: regionprof.py <kernel.js> [reps] [otter-binary]
Runs `otter run --jit-artifacts` on the kernel wrapped in a reps loop, samples
the process with `sample`, and attributes every sample whose leaf frame is an
unknown (JIT) address to the code object and code-map region containing it.
Native leaf frames are reported by symbol.
"""
import json, os, pathlib, re, subprocess, sys, tempfile, time, collections

kernel = sys.argv[1]
reps = int(sys.argv[2]) if len(sys.argv) > 2 else 400
binary = sys.argv[3] if len(sys.argv) > 3 else "/Users/alexanderstreltsov/work/octofhir/otter/target/release/otter"
tmp = pathlib.Path(tempfile.mkdtemp(prefix="regionprof-", dir=os.environ.get("TMPDIR", "/tmp")))
script = tmp / "k.js"
script.write_text(pathlib.Path(kernel).read_text() + f"\nlet __r=0; for (let __i=0; __i<{reps}; __i++) __r=engineKernel(); console.log(__r);\n")
art = tmp / "art"
proc = subprocess.Popen([binary, "run", f"--jit-artifacts={art}", str(script)], stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
time.sleep(float(os.environ.get("DELAY", "2.0")))
out = tmp / "sample.txt"
subprocess.run(["sample", str(proc.pid), os.environ.get("SECS", "4"), "-file", str(out)], capture_output=True)
proc.wait()

# code objects: runtime range + regions
objects = []
for d in sorted(art.glob("jit-*")):
    cm = json.loads((d / "code-map.json").read_text())
    man = json.loads((d / "manifest.json").read_text())
    rng = cm.get("runtimeAddressRange") or {}
    if "start" not in rng:
        continue
    start = int(rng["start"], 16); end = int(rng["endExclusive"], 16)
    regions = [r for r in cm["regions"] if r["kind"] != "machineScalarFunction"]
    objects.append((start, end, f'{man["functionName"]}/{man["tier"][0]}{d.name.split("-c")[-1]}', regions))

text = out.read_text()
graph = text.split("Call graph:", 1)[1].split("Total number in stack", 1)[0]
# self time per frame name from the indented call graph
entries = []
for line in graph.splitlines():
    m = re.match(r"^([\s+!:|]*?)(\d+) (.*)$", line)
    if not m:
        continue
    entries.append((len(m.group(1)), int(m.group(2)), m.group(3)))
selfc = collections.Counter()
for i, (depth, count, name) in enumerate(entries):
    child = 0
    for d2, c2, _ in entries[i + 1:]:
        if d2 <= depth:
            break
        if all(d2 > d3 for d3, _, _ in []):
            pass
    # children = following entries with the smallest depth greater than ours until depth<=ours
    j = i + 1; kid_depth = None
    while j < len(entries) and entries[j][0] > depth:
        if kid_depth is None:
            kid_depth = entries[j][0]
        if entries[j][0] == kid_depth:
            child += entries[j][1]
        j += 1
    if "Thread_" in name:
        continue
    selfc[name] += count - child
by_region = collections.Counter(); by_native = collections.Counter(); jit_total = 0; total = 0
for name, count in selfc.items():
    if count <= 0:
        continue
    total += count
    a = re.search(r"\[0x([0-9a-f]+)\]", name)
    if name.startswith("???") and a:
        addr = int(a.group(1), 16)
        jit_total += count
        for start, end, label, regions in objects:
            if start <= addr < end:
                off = addr - start
                kind = "(unattributed)"
                for r in regions:
                    if r["startOffset"] <= off < r["endOffset"]:
                        kind = f'{r["kind"]}@{r.get("bytePc", "")}'
                        break
                by_region[f"{label} {kind}"] += count
                break
        else:
            by_region["(unknown code)"] += count
    else:
        by_native[re.sub(r"\s*\(in .*", "", name)[:110]] += count
print(f"total leaf samples {total} (collapsed >=5 only); jit {jit_total}")
for k, v in by_region.most_common(40):
    print(f"{v:6d} {100*v/max(total,1):5.1f}%  {k}")
print("--- native")
for k, v in by_native.most_common(15):
    print(f"{v:6d} {100*v/max(total,1):5.1f}%  {k}")
print("artifacts:", art)
