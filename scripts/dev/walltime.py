#!/usr/bin/env python3
"""Wall-clock medians of the fixed-work corpus with hyperfine, sequentially.

usage: walltime.py <out-dir> [--engines otter node] [--workloads fib ts ...]
                   [--runs 5] [--otter target/release/otter]

Writes `<out-dir>/<workload>.json` (hyperfine export) per workload and prints
one line per workload with each engine's median, min and user time. Run it on
a quiet machine: no cargo, no other measurement.
"""
import argparse
import json
from pathlib import Path
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[2]
WORKLOADS = {
    "fib": "benchmarks/results/calls/fib.js",
    "mega_method": "benchmarks/results/calls/mega_method.js",
    "ast_ctor": "benchmarks/results/calls/ast_ctor.js",
    "crypto": "benchmarks/results/slice11/crypto-fixed.js",
    "earley-boyer": "benchmarks/results/octane_fixed/earley-boyer-x1.js",
    "zlib": "benchmarks/results/slice11/zlib-fixed.js",
    "ts": "benchmarks/results/slice11/ts-fixed.js",
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("out", type=Path)
    parser.add_argument("--otter", type=Path, default=ROOT / "target/release/otter")
    parser.add_argument("--engines", nargs="+", choices=["otter", "node"], default=["otter"])
    parser.add_argument("--workloads", nargs="+", choices=list(WORKLOADS), default=list(WORKLOADS))
    parser.add_argument("--runs", type=int, default=5)
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=False)
    commands = {"otter": [str(args.otter.resolve()), "run"], "node": [shutil.which("node")]}
    for workload in args.workloads:
        script = WORKLOADS[workload]
        export = args.out / f"{workload}.json"
        cmd = ["hyperfine", "-N", "-w", "1", "-r", str(args.runs), "--export-json", str(export)]
        cmd += [" ".join(commands[e] + [script]) for e in args.engines]
        subprocess.run(cmd, cwd=ROOT, check=True, capture_output=True)
        results = json.loads(export.read_text())["results"]
        cells = [f"{e} {r['median']:.3f}s (min {r['min']:.3f}, user {r['user']:.3f})"
                 for e, r in zip(args.engines, results)]
        print(f"{workload:13} " + " | ".join(cells), flush=True)


if __name__ == "__main__":
    main()
