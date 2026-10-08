#!/usr/bin/env python3
"""Whole-process time profile of short command launches, via xctrace.

usage: launchprof.py <out-dir> <runs> -- <command...>
Records each launch with the Time Profiler template from its first
instruction (`sample` attaches too late for a short process), exports the
time-profile table and aggregates, over every run, self samples (leaf frame)
and inclusive samples (any frame, counted once per sample) by function name,
for the `otter-isolate` thread unless THREAD names another. Prints the top
rows of both tables. FOCUS=<substring> also aggregates the direct callers and
callees of the outermost frame matching it. WITHIN=<substring> keeps only
the samples with a frame matching it, profiling that subtree alone. Traces and exports stay under
<out-dir>. Run otter with `--perf-basic-prof` to name generated code: the
launch's `/tmp/perf-<pid>.map` is kept beside its export and resolves bare
JIT addresses to `JS:<function> [<tier> c<code-object>]`; OFFSETS=1 keeps the
code offset (`+0x..`) to read against that run's `--jit-artifacts` listing.
BOUNDARY=1 also counts, per sample, the native frame the innermost generated
frame called: the runtime entries compiled code pays for.
"""
import bisect, collections, glob, os, pathlib, shutil, subprocess, sys, time
import xml.etree.ElementTree as ET


def load_perf_map(path):
    """Sorted (start, end, name) ranges of a perf map, or an empty list."""
    ranges = []
    if path.exists():
        for line in path.read_text().splitlines():
            start, size, name = line.split(" ", 2)
            ranges.append((int(start, 16), int(start, 16) + int(size, 16), name))
    ranges.sort()
    return ranges


def symbolize(name, ranges, starts):
    if not ranges or not name.startswith("0x"):
        return name
    address = int(name, 16)
    index = bisect.bisect_right(starts, address) - 1
    if index >= 0 and address < ranges[index][1]:
        start, _, symbol = ranges[index]
        return f"{symbol}+{address - start:#x}" if offsets else symbol
    return name

out = pathlib.Path(sys.argv[1])
runs = int(sys.argv[2])
command = sys.argv[sys.argv.index("--") + 1:]
thread_filter = os.environ.get("THREAD", "otter-isolate")
top = int(os.environ.get("TOP", "40"))
focus = os.environ.get("FOCUS")
within = os.environ.get("WITHIN")
offsets = os.environ.get("OFFSETS") == "1"
boundary = collections.Counter() if os.environ.get("BOUNDARY") == "1" else None
callers = collections.Counter()
callees = collections.Counter()
out.mkdir(parents=True, exist_ok=True)

self_counts = collections.Counter()
inclusive = collections.Counter()
total = 0
for run in range(runs):
    trace = out / f"run{run}.trace"
    export = out / f"run{run}.xml"
    perf_map = out / f"run{run}.perfmap"
    if not export.exists():
        started = time.time()
        subprocess.run(["xcrun", "xctrace", "record", "--template", "Time Profiler",
                        "--output", str(trace), "--launch", "--", *command],
                       check=True, capture_output=True)
        maps = [m for m in glob.glob("/tmp/perf-*.map") if os.path.getmtime(m) >= started]
        if maps:
            shutil.copy(max(maps, key=os.path.getmtime), perf_map)
        with export.open("w") as handle:
            subprocess.run(["xcrun", "xctrace", "export", "--input", str(trace), "--xpath",
                            '/trace-toc/run[@number="1"]/data/table[@schema="time-profile"]'],
                           check=True, stdout=handle, stderr=subprocess.DEVNULL)
    ranges = load_perf_map(perf_map)
    starts = [start for start, _, _ in ranges]
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
        names = [symbolize(resolve(frame).get("name") or "?", ranges, starts)
                 for frame in backtrace.findall("frame")]
        if not names:
            continue
        if within and not any(within in name for name in names):
            continue
        total += 1
        self_counts[names[0]] += 1
        if boundary is not None:
            generated = [index for index, name in enumerate(names) if name.startswith("JS:")]
            if generated:
                index = generated[0]
                boundary[names[index - 1] if index > 0 else "<generated>"] += 1
        for name in set(names):
            inclusive[name] += 1
        if focus:
            hits = [index for index, name in enumerate(names) if focus in name]
            if hits:
                index = hits[-1]
                callers[names[index + 1] if index + 1 < len(names) else "<root>"] += 1
                callees[names[index - 1] if index > 0 else "<self>"] += 1

print(f"samples on {thread_filter}: {total} over {runs} runs")


def category(name):
    """Coarse owner of a self sample."""
    if name.startswith("JS:"):
        return "jit:" + ("optimizing" if "[optimizing" in name else "template")
    for marker, label in (("otter_gc", "gc"), ("regalloc", "jit-compiler"), ("otter_jit", "jit-compiler"),
                          ("dispatch", "interpreter"), ("otter_compiler", "bytecode-compiler"),
                          ("oxc", "bytecode-compiler"), ("malloc", "malloc"), ("free", "malloc"),
                          ("otter_vm", "vm-runtime"), ("otter_runtime", "vm-runtime")):
        if marker in name:
            return label
    return "other"


categories = collections.Counter()
for name, count in self_counts.items():
    categories[category(name)] += count
print("== categories")
for name, count in categories.most_common():
    print(f"{count:6d} {100 * count / max(total, 1):5.1f}%  {name}")
print("== self")
for name, count in self_counts.most_common(top):
    print(f"{count:6d}  {name[:150]}")
print("== inclusive")
for name, count in inclusive.most_common(top):
    print(f"{count:6d}  {name[:150]}")
if boundary is not None:
    print("== runtime entries called by generated code")
    for name, count in boundary.most_common(top):
        print(f"{count:6d}  {name[:150]}")
if focus:
    for title, table in (("callers", callers), ("callees", callees)):
        print(f"== {title} of {focus}")
        for name, count in table.most_common(top):
            print(f"{count:6d}  {name[:150]}")
