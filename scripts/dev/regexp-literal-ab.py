#!/usr/bin/env python3
"""Alternate frozen before/after binaries and Node over identical fixed work.

macOS time(1) supplies CPU, RSS and retired instructions; the existing capture
helper preserves raw streams and applies a process-group watchdog. The literal
kernel also reports first invocation, warmups and steady samples. Aggregate its
process medians, not dependent invocations. Run without builds or profiling.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import statistics
import subprocess

ROOT = Path(__file__).resolve().parents[2]
spec = importlib.util.spec_from_file_location("fixed_work", ROOT / "scripts/dev/fixed-work.py")
capture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(capture)


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def summary(values):
    return {"median": statistics.median(values), "min": min(values), "max": max(values),
            "stdev": statistics.stdev(values) if len(values) > 1 else 0, "n": len(values)}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--before", type=Path, required=True)
    parser.add_argument("--after", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--runs", type=int, default=5)
    parser.add_argument("--fixtures", nargs="+", default=["startup", "literal", "literal-wide", "regexp", "ts", "crypto", "zlib", "earley-boyer", "richards"])
    args = parser.parse_args()
    if args.runs < 3:
        parser.error("at least three independent processes are required")
    if any(key.startswith("OTTER_") for key in os.environ):
        parser.error("remove Otter diagnostic/stress overrides before timing")
    args.out.mkdir(parents=True, exist_ok=False)
    engines = {"before": str(args.before.resolve()), "after": str(args.after.resolve()), "node": shutil.which("node")}
    hashes = {name: digest(path) for name, path in engines.items()}
    sources = {name: ROOT / ({"literal": "benchmarks/scripts/regexp-literal.js", "literal-wide": "benchmarks/scripts/regexp-literal-wide.js", "startup": "benchmarks/scripts/regexp-startup.js"}.get(name, f"benchmarks/fixtures/fixed-work/{name}.js")) for name in args.fixtures}
    source_hashes = {name: digest(path) for name, path in sources.items()}
    report = {"status": "running", "binarySha256": hashes, "sourceSha256": source_hashes,
              "versions": {name: subprocess.check_output([path, "--version"], text=True).strip() for name, path in engines.items()}, "runs": []}
    output = args.out / "results.json"
    def save():
        output.write_text(json.dumps(report, indent=2) + "\n")
    save()
    for name, source in sources.items():
        expected = None
        for iteration in range(args.runs + 1):
            (args.out / f"{name}-{iteration}-processes.txt").write_text(subprocess.check_output(["ps", "-Ao", "pid,pcpu,etime,comm", "-r"], text=True))
            order = list(engines)
            order = order[iteration % 3:] + order[:iteration % 3]
            for engine in order:
                command = [engines[engine], *([] if engine == "node" else ["--timeout", "0", "run"]), str(source)]
                prefix = args.out / f"{name}-{iteration}-{engine}"
                run = capture.capture(command, prefix, 120)
                run.update(fixture=name, engine=engine, warmup=iteration == 0)
                stdout = prefix.with_suffix(".stdout").read_bytes()
                capture.validate_sample(run, stdout, None, "exact")
                if run["status"] != "ok":
                    report["runs"].append(run)
                    report["status"] = "failed"
                    save()
                    raise RuntimeError(run)
                if name in ("literal", "literal-wide"):
                    data = json.loads(stdout)
                    run.update(firstMs=data["warmupMs"][0], steadyMs=statistics.median(data["samplesMs"]))
                    identity = data["checksum"]
                    assert len(data["warmupMs"]) == 6 and len(data["samplesMs"]) == 10
                else:
                    identity = stdout
                if expected is None:
                    expected = identity
                if identity != expected:
                    raise RuntimeError(f"different work/result: {name}/{engine}")
                report["runs"].append(run)
                save()
            print(f"{name}: process block {iteration}/{args.runs}", flush=True)
    assert hashes == {name: digest(path) for name, path in engines.items()}
    assert source_hashes == {name: digest(path) for name, path in sources.items()}
    report["summary"] = {}
    for name in sources:
        report["summary"][name] = {}
        for engine in engines:
            rows = [r for r in report["runs"] if r["fixture"] == name and r["engine"] == engine and not r["warmup"]]
            metrics = [*capture.METRICS, *(["firstMs", "steadyMs"] if name in ("literal", "literal-wide") else [])]
            report["summary"][name][engine] = {key: summary([r[key] for r in rows if r.get(key) is not None]) for key in metrics if any(r.get(key) is not None for r in rows)}
    report["status"] = "passed"
    save()
    for name, engines in report["summary"].items():
        print(name, {engine: round(row["launchToExitSeconds"]["median"] * 1000, 2) for engine, row in engines.items()}, flush=True)


if __name__ == "__main__":
    main()
