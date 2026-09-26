#!/usr/bin/env python3
"""Profile one Octane suite by JIT function, runtime boundary and native leaf.

Usage: scripts/dev/octaneprof.py <suite> [tiered|jitless|interpreter] [secs] [repeat]

Runs the suite (prelude + base.js + suite files + a driver that repeats
BenchmarkSuite.RunSuites `repeat` times) with --jit-artifacts, samples the
isolate thread with macOS `sample` after a warmup delay, and attributes every
sample of the `otter-isolate` thread:

  * JIT leaf       — innermost generated code object (function/tier) that
                     owns the leaf PC, split by code-map region kind;
  * JIT -> native  — the first native frame called from generated code (the
                     runtime stub / helper boundary) for native leaves below
                     a generated frame;
  * native only    — the top native symbol for stacks without generated code
                     (interpreter, GC, compiler).

Environment: OTTER_BIN, DELAY (seconds before sampling, default 3), WAIT (exit
timeout), REUSE=1 (re-analyze the previous sample and artifacts).
"""
import collections
import json
import os
import pathlib
import re
import subprocess
import sys
import time

ROOT = pathlib.Path(__file__).resolve().parents[2]
suite = sys.argv[1]
mode = sys.argv[2] if len(sys.argv) > 2 else "tiered"
secs = sys.argv[3] if len(sys.argv) > 3 else "8"
repeat = int(sys.argv[4]) if len(sys.argv) > 4 else 30
binary = os.environ.get("OTTER_BIN", str(ROOT / "target/release/otter"))
octane = ROOT / "benchmarks/.suite-cache/octane"
work = ROOT / "benchmarks/results/octaneprof" / f"{suite}-{mode}"
work.mkdir(parents=True, exist_ok=True)

files = {
    "gbemu": ["gbemu-part1.js", "gbemu-part2.js"],
    "zlib": ["zlib.js", "zlib-data.js"],
    "typescript": ["typescript.js", "typescript-input.js", "typescript-compiler.js"],
}.get(suite, [f"{suite}.js"])
prelude = work / "prelude.js"
prelude.write_text(
    'if (typeof print === "undefined") globalThis.print = function (s) { console.log(s); };\n'
    'if (typeof read === "undefined") globalThis.read = function (n) { throw new Error("read " + n); };\n'
)
driver = work / "driver.js"
driver.write_text(
    "BenchmarkSuite.config.doWarmup = undefined;\n"
    "BenchmarkSuite.config.doDeterministic = undefined;\n"
    f"for (let r = 0; r < {repeat}; r++) BenchmarkSuite.RunSuites({{ NotifyResult() {{}}, "
    "NotifyError(n, e) { print(n + ': ' + e); }, NotifyScore(s) { print('Score: ' + s); } });\n"
)
paths = [str(prelude), str(octane / "base.js")] + [str(octane / f) for f in files] + [str(driver)]
art = work / "art"
out = work / "sample.txt"
if not os.environ.get("REUSE"):
    subprocess.run(["rm", "-rf", str(art)])
    flags = {"tiered": [], "jitless": ["--jitless"], "interpreter": ["--interpreter"]}[mode]
    proc = subprocess.Popen([binary, *flags, f"--jit-artifacts={art}", "run", *paths],
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
    time.sleep(float(os.environ.get("DELAY", "3")))
    sampled = subprocess.run(["sample", str(proc.pid), secs, "-file", str(out)], capture_output=True, text=True)
    if not out.exists():
        sys.exit(f"sample failed (process exited?): {sampled.stderr.strip()} {proc.poll()}")
    # Artifacts (code maps with runtime address ranges) are written at exit, so
    # the run must finish on its own: pick `repeat` to outlast DELAY + secs.
    try:
        proc.wait(timeout=float(os.environ.get("WAIT", "600")))
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()


objects = []
for d in sorted(art.glob("jit-*")):
    try:
        cm = json.loads((d / "code-map.json").read_text())
        man = json.loads((d / "manifest.json").read_text())
    except (OSError, json.JSONDecodeError):
        continue
    rng = cm.get("runtimeAddressRange") or {}
    if "start" not in rng:
        continue
    start = int(rng["start"], 16)
    end = int(rng["endExclusive"], 16)
    regions = [r for r in cm["regions"] if r["kind"] != "machineScalarFunction"]
    label = f'{man["functionName"]}#{man.get("functionId", "?")}/{man["tier"][0]}'
    objects.append((start, end, label, regions))


def jit_label(addr):
    for start, end, label, regions in objects:
        if start <= addr < end:
            off = addr - start
            for r in regions:
                if r["startOffset"] <= off < r["endOffset"]:
                    return label, r["kind"]
            return label, "(unattributed)"
    return None


text = out.read_text()
graph = text.split("Call graph:", 1)[1].split("Total number in stack", 1)[0]
lines = graph.splitlines()
start_idx = next(i for i, l in enumerate(lines) if "otter-isolate" in l)
entries = []
for line in lines[start_idx + 1:]:
    m = re.match(r"^([\s+!:|]*?)(\d+) (.*)$", line)
    if not m:
        continue
    depth = len(m.group(1))
    if "Thread_" in m.group(3):
        break
    entries.append((depth, int(m.group(2)), m.group(3)))


def clean(name):
    name = re.sub(r"\s*\(in [^)]*\).*", "", name)
    name = re.sub(r"\s+\+ \d+.*", "", name)
    name = re.sub(r"_RN[A-Za-z0-9_]*?(otter_[a-z_]+)\d*", r"\1::", name)
    return name.strip()[:100]


def frame(name):
    a = re.search(r"\[0x([0-9a-f]+)", name)
    if name.startswith("???") and a:
        return ("jit", jit_label(int(a.group(1), 16)) or ("(unknown code)", ""))
    return ("native", clean(name))


jit_leaf = collections.Counter()
jit_leaf_fn = collections.Counter()
boundary = collections.Counter()
native_only = collections.Counter()
total = 0
stack = []  # (depth, frame)
for i, (depth, count, name) in enumerate(entries):
    while stack and stack[-1][0] >= depth:
        stack.pop()
    fr = frame(name)
    stack.append((depth, fr))
    child = 0
    kid_depth = None
    j = i + 1
    while j < len(entries) and entries[j][0] > depth:
        if kid_depth is None:
            kid_depth = entries[j][0]
        if entries[j][0] == kid_depth:
            child += entries[j][1]
        j += 1
    selfc = count - child
    if selfc <= 0:
        continue
    total += selfc
    frames = [f for _, f in stack]
    last_jit = max((k for k, f in enumerate(frames) if f[0] == "jit"), default=None)
    if fr[0] == "jit":
        label, region = fr[1]
        jit_leaf[f"{label} {region}"] += selfc
        jit_leaf_fn[label] += selfc
    elif last_jit is not None:
        entry = frames[last_jit + 1][1] if last_jit + 1 < len(frames) else fr[1]
        boundary[f"{frames[last_jit][1][0]} -> {entry}"] += selfc
    else:
        native_only[fr[1]] += selfc

jit_total = sum(jit_leaf.values())
bnd_total = sum(boundary.values())
nat_total = sum(native_only.values())
print(f"{suite} {mode}: isolate samples {total}; jit leaf {jit_total} ({100*jit_total/max(total,1):.1f}%), "
      f"jit->native {bnd_total} ({100*bnd_total/max(total,1):.1f}%), native only {nat_total} "
      f"({100*nat_total/max(total,1):.1f}%)")
print("--- JIT leaf by function")
for k, v in jit_leaf_fn.most_common(20):
    print(f"{v:6d} {100*v/max(total,1):5.1f}%  {k}")
print("--- JIT leaf by region")
for k, v in jit_leaf.most_common(20):
    print(f"{v:6d} {100*v/max(total,1):5.1f}%  {k}")
print("--- generated code -> native boundary")
by_entry = collections.Counter()
for k, v in boundary.items():
    by_entry[k.split(" -> ", 1)[1]] += v
for k, v in by_entry.most_common(20):
    print(f"{v:6d} {100*v/max(total,1):5.1f}%  {k}")
print("--- boundary by caller")
for k, v in boundary.most_common(20):
    print(f"{v:6d} {100*v/max(total,1):5.1f}%  {k}")
print("--- native only (top leaf)")
for k, v in native_only.most_common(20):
    print(f"{v:6d} {100*v/max(total,1):5.1f}%  {k}")
print("artifacts:", art)
