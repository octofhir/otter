#!/usr/bin/env python3
"""Whole-process time profile of short command launches, via xctrace.

usage: launchprof.py <out-dir> <runs> -- <command...>
Records each launch with the Time Profiler template from its first
instruction (`sample` attaches too late for a short process), exports the
time-profile table and aggregates, over every run, self samples (leaf frame)
and inclusive samples (any frame, counted once per sample) by function name,
for the `otter-isolate` thread unless THREAD names another. Prints the top
rows of both tables. Traces and exports stay under <out-dir>.
"""
import collections, os, pathlib, subprocess, sys
import xml.etree.ElementTree as ET

out = pathlib.Path(sys.argv[1])
runs = int(sys.argv[2])
command = sys.argv[sys.argv.index("--") + 1:]
thread_filter = os.environ.get("THREAD", "otter-isolate")
top = int(os.environ.get("TOP", "40"))
out.mkdir(parents=True, exist_ok=True)

self_counts = collections.Counter()
inclusive = collections.Counter()
total = 0
for run in range(runs):
    trace = out / f"run{run}.trace"
    export = out / f"run{run}.xml"
    if not export.exists():
        subprocess.run(["xcrun", "xctrace", "record", "--template", "Time Profiler",
                        "--output", str(trace), "--launch", "--", *command],
                       check=True, capture_output=True)
        with export.open("w") as handle:
            subprocess.run(["xcrun", "xctrace", "export", "--input", str(trace), "--xpath",
                            '/trace-toc/run[@number="1"]/data/table[@schema="time-profile"]'],
                           check=True, stdout=handle, stderr=subprocess.DEVNULL)
    root = ET.parse(export).getroot()
    by_id = {}
    for element in root.iter():
        identifier = element.get("id")
        if identifier is not None:
            by_id[identifier] = element

    def resolve(element):
        reference = element.get("ref")
        return by_id[reference] if reference is not None else element

    for row in root.iter("row"):
        thread = row.find("thread")
        if thread is None:
            continue
        thread = resolve(thread)
        if thread_filter not in (thread.get("fmt") or ""):
            continue
        stack = row.find("tagged-backtrace")
        if stack is None:
            stack = row.find("backtrace")
        if stack is None:
            continue
        stack = resolve(stack)
        backtrace = stack.find("backtrace") if stack.tag == "tagged-backtrace" else stack
        if backtrace is None:
            continue
        backtrace = resolve(backtrace)
        names = [resolve(frame).get("name") or "?" for frame in backtrace.findall("frame")]
        if not names:
            continue
        total += 1
        self_counts[names[0]] += 1
        for name in set(names):
            inclusive[name] += 1

print(f"samples on {thread_filter}: {total} over {runs} runs")
print("== self")
for name, count in self_counts.most_common(top):
    print(f"{count:6d}  {name[:150]}")
print("== inclusive")
for name, count in inclusive.most_common(top):
    print(f"{count:6d}  {name[:150]}")
