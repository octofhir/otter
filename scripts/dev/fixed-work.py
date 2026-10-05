#!/usr/bin/env python3
"""Measure fixed work with macOS time(1) in fresh, sequential processes.

Keep commands, raw outputs, counters and executable/source hashes. Explicit
warmup processes are excluded from aggregates. Validate stdout across engines
and repetitions. Run on an idle machine without compilation or profiling.
"""
import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import re
import shutil
import signal
import statistics
import subprocess
import sys
import threading
import time

ROOT = Path(__file__).resolve().parents[2]
WORKLOADS = {
    "ts": "benchmarks/results/slice11/ts-fixed.js",
    "zlib": "benchmarks/results/slice11/zlib-fixed.js",
    "crypto": "benchmarks/results/slice11/crypto-fixed.js",
    "fib": "benchmarks/results/calls/fib.js",
    "mega_method": "benchmarks/results/calls/mega_method.js",
    "ast_ctor": "benchmarks/results/calls/ast_ctor.js",
    "earley-boyer": "benchmarks/results/octane_fixed/earley-boyer-x1.js",
}
COUNTER_METRICS = ("real_seconds", "user_seconds", "sys_seconds", "instructions", "rss_bytes")
METRICS = COUNTER_METRICS + ("launchToExitSeconds",)


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2) + "\n")


def parse_counters(counters):
    parsed = {metric: None for metric in COUNTER_METRICS}
    number = r"([0-9]+(?:\.[0-9]+)?)"
    match = re.search(r"^\s*" + number + r"\s+real\s+" + number +
                      r"\s+user\s+" + number + r"\s+sys\s*$",
                      counters, re.M)
    if match:
        for key, value in zip(METRICS[:3], match.groups()):
            number = float(value)
            if math.isfinite(number) and number >= 0:
                parsed[key] = number
    for label, key in [("instructions retired", "instructions"),
                       ("maximum resident set size", "rss_bytes")]:
        match = re.search(r"^\s*(\d+)\s+" + label + r"\s*$", counters, re.M)
        if match:
            parsed[key] = int(match[1])
    return parsed


def normalize_stdout(output, normalization):
    if normalization == "exact":
        return output
    if normalization == "line-endings":
        return output.replace(b"\r\n", b"\n").replace(b"\r", b"\n")
    raise ValueError(f"Unknown stdout normalization: {normalization}")


def summarize(samples):
    valid = [sample for sample in samples if sample["status"] == "ok"]
    summaries = {}
    for metric in METRICS:
        values = [sample[metric] for sample in valid if sample[metric] is not None]
        summaries[metric] = ({"min": min(values), "median": statistics.median(values),
                              "max": max(values), "count": len(values)}
                             if values else {"min": None, "median": None, "max": None, "count": 0})
    return summaries


def build_command(engine, executable, source):
    command = [executable] + (["run"] if engine in ("otter", "otter-reference") else [])
    if engine == "bun":
        # Legacy fixtures need Script semantics; disclose and include loader cost.
        command += ["-e", '(0, eval)(require("node:fs").readFileSync(process.argv[1], "utf8"))']
    return command + [str(source)]


def rotated_engines(engines, offset):
    offset %= len(engines)
    return engines[offset:] + engines[:offset]


def source_provenance():
    """Identify the current tree without claiming it proves a binary's origin."""
    head = subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip()
    status = subprocess.check_output(
        ["git", "status", "--porcelain=v1", "--untracked-files=all"], cwd=ROOT, text=True)
    patch = subprocess.check_output(["git", "diff", "--binary", "HEAD"], cwd=ROOT)
    paths = subprocess.check_output(
        ["git", "ls-files", "--others", "--exclude-standard", "-z"], cwd=ROOT)
    untracked = {os.fsdecode(path): digest(ROOT / os.fsdecode(path))
                 for path in sorted(path for path in paths.split(b"\0") if path)}
    tree = hashlib.sha256(head.encode() + b"\0" + patch + b"\0" +
                          json.dumps(untracked, sort_keys=True).encode()).hexdigest()
    return {"head": head, "dirty": bool(status), "status": status.splitlines(),
            "patchSha256": hashlib.sha256(patch).hexdigest(), "untrackedSha256": untracked,
            "treeSha256": tree, "executableSourceRelation": "unverified"}


def capture(command, prefix, timeout):
    """Apply an outer watchdog to time(1) and its whole child process group."""
    timed_out = False
    completed = threading.Event()
    watchdog_lock = threading.Lock()
    with prefix.with_suffix(".stdout").open("wb") as out, \
            prefix.with_suffix(".time").open("wb") as err:
        started = time.perf_counter()
        process = subprocess.Popen(["/usr/bin/time", "-l", *command], cwd=ROOT,
                                   stdout=out, stderr=err, start_new_session=True)
        def expire():
            nonlocal timed_out
            # Serialize completion with killing, and never kill after this
            # capture has returned or after a naturally completed process.
            with watchdog_lock:
                if completed.is_set() or process.poll() is not None:
                    return
                timed_out = True
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass

        watchdog = threading.Timer(max(0, timeout - (time.perf_counter() - started)), expire)
        watchdog.daemon = True
        watchdog.start()
        try:
            # Blocking wait avoids wait(timeout)'s exponential polling delay.
            code = process.wait()
            finished = time.perf_counter()
        finally:
            with watchdog_lock:
                completed.set()
            watchdog.cancel()
            watchdog.join()
    counters = parse_counters(prefix.with_suffix(".time").read_text(errors="replace"))
    return {"command": command, "exit": code, "timedOut": timed_out,
            "launchToExitSeconds": finished - started,
            "watchdogElapsedSeconds": time.perf_counter() - started,
            "stdoutFile": prefix.with_suffix(".stdout").name,
            "stderrAndCountersFile": prefix.with_suffix(".time").name, **counters}


def validate_sample(sample, output, expected, normalization):
    normalized = normalize_stdout(output, normalization)
    sample["stdoutSha256"] = hashlib.sha256(output).hexdigest()
    sample["normalizedStdoutSha256"] = hashlib.sha256(normalized).hexdigest()
    sample["matchesExpectedStdout"] = expected is None or normalized == expected
    failures = []
    if sample["timedOut"]:
        failures.append("external watchdog timeout")
    if sample["exit"] != 0:
        failures.append(f"exit {sample['exit']}")
    missing = [metric for metric in METRICS if sample[metric] is None]
    if missing:
        failures.append("missing counters: " + ", ".join(missing))
    if not sample["matchesExpectedStdout"]:
        failures.append("stdout mismatch")
    sample.update(status="failed" if failures else "ok", failures=failures)
    return normalized


def refresh_rows(rows, runs, warmup):
    for row in rows:
        row["summaries"] = summarize(row["samples"])
        row.update({metric: row["summaries"][metric]["median"] for metric in METRICS})
        observed = row["warmup_samples"] + row["samples"]
        row["exit"] = next((sample["exit"] for sample in observed if sample["exit"] != 0),
                           0 if observed else None)
        failed = row["failures"] or any(sample["status"] != "ok" for sample in observed)
        complete = len(row["samples"]) == runs and len(row["warmup_samples"]) == warmup
        row["status"] = "failed" if failed else "ok" if complete else "incomplete"
        row["scoreable"] = row["status"] == "ok"


def nonnegative_int(value):
    number = int(value)
    if number < 0:
        raise argparse.ArgumentTypeError("must be at least 0")
    return number


def positive_int(value):
    number = nonnegative_int(value)
    if number == 0:
        raise argparse.ArgumentTypeError("must be at least 1")
    return number


def positive_float(value):
    number = float(value)
    if not math.isfinite(number) or number <= 0:
        raise argparse.ArgumentTypeError("must be finite and greater than 0")
    return number


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("out", type=Path)
    parser.add_argument("--otter", type=Path, default=ROOT / "target/release/otter",
                        help="Preserved executable for revision comparisons")
    parser.add_argument("--reference-otter", type=Path,
                        help="Preserved reference executable; select otter-reference in --engines")
    parser.add_argument("--engines", nargs="+", choices=["otter", "otter-reference", "node", "bun"],
                        default=["otter", "node", "bun"])
    parser.add_argument("--workloads", nargs="+", choices=list(WORKLOADS), default=list(WORKLOADS))
    parser.add_argument("--runs", type=positive_int, default=5,
                        help="Measured fresh-process samples per workload/engine (default: 5)")
    parser.add_argument("--warmup", type=nonnegative_int, default=1,
                        help="Excluded fresh-process warmups; no persistent JIT state (default: 1)")
    parser.add_argument("--timeout", type=positive_float, default=120,
                        help="External watchdog in seconds per process (default: 120)")
    parser.add_argument("--stdout-normalization", choices=["exact", "line-endings"], default="exact")
    args = parser.parse_args()
    if len(set(args.engines)) != len(args.engines) or len(set(args.workloads)) != len(args.workloads):
        parser.error("engines and workloads must not contain duplicates")
    args.out.mkdir(parents=True, exist_ok=False)
    engines = {"otter": str(args.otter.resolve()),
               "otter-reference": str(args.reference_otter.resolve()) if args.reference_otter else None,
               "node": shutil.which("node"), "bun": shutil.which("bun")}
    rows = [{"workload": workload, "engine": engine,
             "command": build_command(engine, engines[engine], ROOT / WORKLOADS[workload])
             if engines[engine] else None,
             "samples": [], "warmup_samples": [], "failures": []}
            for workload in args.workloads for engine in args.engines]
    by_key = {(row["workload"], row["engine"]): row for row in rows}
    metadata = {"source": source_provenance(), "uname": list(os.uname()),
                "stress": os.environ.get("OTTER_GC_STRESS"), "gcVerify": os.environ.get("OTTER_GC_VERIFY"),
                "sampling": {"runs": args.runs, "excludedWarmupRuns": args.warmup,
                             "processReuse": "fresh", "engineOrder": "rotated-by-round",
                             "timeoutSeconds": args.timeout},
                "validation": {"stdoutNormalization": args.stdout_normalization,
                               "oracle": "first successful sample; cross-engine agreement only"},
                "metricScope": "whole process including startup, source loading and compilation",
                "timingWindows": {
                    "real_seconds": "time(1) child elapsed time at its printed precision",
                    "launchToExitSeconds": "perf_counter immediately before Popen through blocking wait completion; "
                                           "includes subprocess spawn and /usr/bin/time wrapper; excludes output file "
                                           "setup, watchdog cleanup and counter parsing"},
                "bunLoader": "indirect eval of exact source bytes; wrapper cost included",
                "executables": {}, "scripts": {}, "failures": [], "status": "incomplete",
                "scoreable": False, "baselineEligible": False}
    try:
        metadata["uptime"] = subprocess.check_output(["uptime"], text=True).strip()
    except (OSError, subprocess.CalledProcessError) as error:
        metadata.update(uptime=None, uptimeUnavailable=str(error))
    for engine in args.engines:
        executable = engines[engine]
        try:
            if engine == "otter-reference" and executable is None:
                raise FileNotFoundError("otter-reference requires --reference-otter PATH")
            if executable is None or not Path(executable).is_file() or not os.access(executable, os.X_OK):
                raise FileNotFoundError(f"{engine} executable unavailable: {executable}")
            version = subprocess.check_output([executable, "--version"], text=True,
                                              timeout=args.timeout).strip()
            metadata["executables"][engine] = {"path": executable, "sha256": digest(executable),
                                                "version": version, "sourceRevision": None,
                                                "buildProfile": "unverified"}
        except (OSError, subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
            metadata["executables"][engine] = {"path": executable, "status": "unavailable", "failure": str(error)}
            metadata["failures"].append(f"{engine}: {error}")
            for row in rows:
                if row["engine"] == engine:
                    row["failures"].append(f"unavailable engine: {error}")
    for workload in args.workloads:
        try:
            metadata["scripts"][workload] = {"path": WORKLOADS[workload], "sha256": digest(ROOT / WORKLOADS[workload])}
        except OSError as error:
            metadata["failures"].append(f"{workload}: {error}")
            for row in rows:
                if row["workload"] == workload:
                    row["failures"].append(f"unavailable workload: {error}")
    if sys.platform != "darwin":
        metadata["failures"].append("macOS time -l counters require Darwin")
    if metadata["stress"] not in (None, "0") or metadata["gcVerify"] not in (None, "0"):
        metadata["failures"].append("GC stress/verification must be disabled for performance capture")

    def save():
        refresh_rows(rows, args.runs, args.warmup)
        write_json(args.out / "environment.json", metadata)
        write_json(args.out / "results.json", rows)

    def fail(message):
        metadata["failures"].append(message)
        metadata["status"] = "failed"
        for row in rows:
            row["failures"].append("capture failed: " + message)
        save()
        print("Incomplete measurement: " + "; ".join(metadata["failures"]), file=sys.stderr)
        return 1

    save()
    if metadata["failures"]:
        return fail("preflight failed; no samples run")
    for index, workload in enumerate(args.workloads):
        expected = None
        for phase, count in [("warmup", args.warmup), ("measured", args.runs)]:
            for round_index in range(count):
                for engine in rotated_engines(args.engines, index + round_index):
                    row = by_key[(workload, engine)]
                    prefix = args.out / f"{workload}-{engine}-{phase}-{round_index + 1:04d}"
                    print(f"START {workload} {engine} {phase} {round_index + 1}/{count}", flush=True)
                    try:
                        sample = capture(row["command"], prefix, args.timeout)
                        sample.update(phase=phase, round=round_index + 1)
                        output = prefix.with_suffix(".stdout").read_bytes()
                        normalized = validate_sample(sample, output, expected, args.stdout_normalization)
                    except OSError as error:
                        row["failures"].append(str(error))
                        return fail(f"{prefix.name}: {error}")
                    row["warmup_samples" if phase == "warmup" else "samples"].append(sample)
                    if sample["status"] == "ok" and expected is None:
                        expected = normalized
                        (args.out / f"{workload}.expected.stdout").write_bytes(output)
                        metadata["scripts"][workload].update(
                            expectedStdoutSha256=hashlib.sha256(expected).hexdigest(), expectedStdoutEngine=engine)
                    save()
                    if sample["status"] != "ok":
                        return fail(f"{prefix.name}: " + "; ".join(sample["failures"]))
    try:
        metadata["sourceAfter"] = source_provenance()
        if metadata["sourceAfter"]["treeSha256"] != metadata["source"]["treeSha256"]:
            metadata["failures"].append("source tree changed during measurement")
        for engine, executable in metadata["executables"].items():
            if digest(executable["path"]) != executable["sha256"]:
                metadata["failures"].append(f"{engine} executable changed during measurement")
        for workload, source in metadata["scripts"].items():
            if digest(ROOT / source["path"]) != source["sha256"]:
                metadata["failures"].append(f"{workload} source changed during measurement")
    except OSError as error:
        metadata["failures"].append(f"provenance recheck failed: {error}")
    if metadata["failures"]:
        for row in rows:
            row["failures"].extend(metadata["failures"])
        return fail("measurement provenance changed")
    metadata.update(status="ok", scoreable=True)
    save()
    for row in rows:
        print(json.dumps({key: row[key] for key in ["workload", "engine", "status", *METRICS]}), flush=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
