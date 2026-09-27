#!/usr/bin/env python3
"""Instruction-level JIT profile of a whole script.

usage: hotasm.py <script.js> <out-dir> [objects]
Runs `otter run --jit-artifacts=<out-dir>/art` on the script, samples it with
`sample` (DELAY/SECS env, default 6s/8s), attributes every JIT leaf sample to
its code object, and writes `<out-dir>/hot-<n>.txt`: that object's asm.txt
with per-instruction sample counts, for the `objects` hottest code objects
(default 3). Prints the per-object totals and hottest code-map regions. An
existing `<out-dir>` with `art/` and `sample.txt` is re-analyzed, not re-run.
"""
import json, os, pathlib, re, subprocess, sys, time, collections

script, outdir = sys.argv[1], pathlib.Path(sys.argv[2])
top = int(sys.argv[3]) if len(sys.argv) > 3 else 3
binary = os.environ.get("OTTER", str(pathlib.Path(__file__).resolve().parents[2] / "target/release/otter"))
outdir.mkdir(parents=True, exist_ok=True)
art = outdir / "art"
sample = outdir / "sample.txt"
if not (art.exists() and sample.exists()):
    command = [binary, "--timeout", "0", "run", f"--jit-artifacts={art}", script]
    (outdir / "command.json").write_text(json.dumps(command, indent=2) + "\n")
    with (outdir / "stdout.txt").open("w") as stdout, (outdir / "stderr.txt").open("w") as stderr:
        proc = subprocess.Popen(command, stdout=stdout, stderr=stderr)
        time.sleep(float(os.environ.get("DELAY", "6")))
        subprocess.run(["sample", str(proc.pid), os.environ.get("SECS", "8"), "-file", str(sample)], capture_output=True)
        # Artifacts are written when the run finishes. Failed or timed-out runs
        # cannot silently become a successful full-work profile.
        status = proc.wait()
    if status:
        raise SystemExit(f"Otter exited {status}; inspect {outdir / 'stderr.txt'}")

objects = []
for d in sorted(art.glob("jit-*")):
    cm = json.loads((d / "code-map.json").read_text())
    rng = cm.get("runtimeAddressRange") or {}
    if "start" in rng:
        objects.append((int(rng["start"], 16), int(rng["endExclusive"], 16), d))

# Exact self samples of the isolate thread, from its indented call graph.
graph = sample.read_text().split("Call graph:", 1)[1].split("Total number in stack", 1)[0]
thread = graph.split("otter-isolate", 1)[1].split("\n    ", 1)[1]
thread = re.split(r"\n    \d+ Thread_", thread, maxsplit=1)[0]
entries = []
for line in thread.splitlines():
    m = re.match(r"^([\s+!:|]*?)(\d+) (.*)$", line)
    if m:
        entries.append((len(m.group(1)), int(m.group(2)), m.group(3)))
hits = collections.defaultdict(collections.Counter)
native = collections.Counter()
total = 0
for i, (depth, count, name) in enumerate(entries):
    children, j = 0, i + 1
    while j < len(entries) and entries[j][0] > depth:
        if entries[j][0] == entries[i + 1][0]:
            children += entries[j][1]
        j += 1
    own = count - children
    if own <= 0:
        continue
    total += own
    m = re.search(r"\?\?\?.*\[0x([0-9a-f]+)\]", name)
    if not m:
        native[re.sub(r"\s*\(in .*", "", name)[:120]] += own
        continue
    addr = int(m.group(1), 16)
    # Evicted code's addresses are reused; the latest compile wins.
    for start, end, d in reversed(objects):
        if start <= addr < end:
            hits[d][addr - start] += own
            break
    else:
        native["(unknown JIT code)"] += own

ranked = sorted(hits.items(), key=lambda item: -sum(item[1].values()))
for rank, (d, counter) in enumerate(ranked[:top]):
    print(f"{sum(counter.values()):7d}  {d.name}")
    lines = []
    for line in (d / "asm.txt").read_text().splitlines():
        m = re.match(r"\s*\+?0x([0-9a-f]+):", line)
        count = counter.get(int(m.group(1), 16), 0) if m else 0
        lines.append(f"{count:5d} {line}")
    (outdir / f"hot-{rank}.txt").write_text("\n".join(lines) + "\n")
    regions = [
        r
        for r in json.loads((d / "code-map.json").read_text())["regions"]
        if r["kind"] != "machineScalarFunction"
    ]
    by_region = collections.Counter()
    for offset, count in counter.items():
        inside = [r for r in regions if r["startOffset"] <= offset < r["endOffset"]]
        best = min(inside, key=lambda r: r["endOffset"] - r["startOffset"], default=None)
        by_region[f'{best["kind"]}@{best.get("bytePc")}' if best else "(unattributed)"] += count
    for region, count in by_region.most_common(12):
        print(f"         {count:6d}  {region}")
for d, counter in ranked[top:top + 10]:
    print(f"{sum(counter.values()):7d}  {d.name}")
print(f"isolate self samples: {total}")
for name, count in native.most_common(25):
    print(f"{count:7d}  {name}")
