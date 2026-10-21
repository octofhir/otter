#!/usr/bin/env python3
"""Run a fixture several times and line up each run's cost with its tiering.

usage: tiering_variance.py <otter-binary> <fixture.js> [runs]

Each run records `--jit-events`; per run prints the retired instructions
(`/usr/bin/time -l`), the wall time, how many optimizing compiles finished
per function, and every exit from optimized code by function and reason.
Runs whose costs differ show which compile histories go with which cost.
"""
import collections, json, os, re, subprocess, sys, tempfile

binary, fixture = sys.argv[1], sys.argv[2]
runs = int(sys.argv[3]) if len(sys.argv) > 3 else 8
out = os.path.join("benchmarks/results/tiering-variance")
os.makedirs(out, exist_ok=True)
for run in range(runs):
    events = os.path.join(out, f"run{run}.json")
    timed = subprocess.run(
        ["/usr/bin/time", "-l", binary, f"--jit-events={events}", fixture],
        stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, text=True,
    ).stderr
    instructions = int(re.search(r"(\d+)\s+instructions retired", timed).group(1))
    wall = float(re.search(r"([\d.]+) real", timed).group(1))
    data = json.load(open(events))
    events_list = data if isinstance(data, list) else data.get("events", [])
    compiles = collections.Counter(
        e.get("functionId") for e in events_list
        if e.get("type") == "compileFinished" and e.get("tier") == "optimizing"
    )
    exits = [
        (e.get("calleeFunctionId"), e.get("exitReason"), e.get("calleeResumePc"))
        for e in events_list
        if e.get("type") == "enteredGenerationDeopt" and e.get("calleeTier") == "optimizing"
    ]
    many = sorted((f, n) for f, n in compiles.items() if n > 1)
    print(f"{instructions / 1e9:6.2f}G {wall:5.2f}s opt={sum(compiles.values()):3d} recompiled={many} exits={exits}")
