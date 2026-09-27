#!/usr/bin/env python3
"""Measure the agreed fixed-work corpus with macOS time(1), sequentially.

Keep stdout, resource counters, executable/script hashes and exact commands.
Use a fresh output directory; profiling and compilation must run separately.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess

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


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("out", type=Path)
    parser.add_argument("--otter", type=Path, default=ROOT / "target/release/otter",
                        help="Preserved executable for revision comparisons")
    parser.add_argument("--engines", nargs="+", choices=["otter", "node", "bun"],
                        default=["otter", "node", "bun"])
    parser.add_argument("--workloads", nargs="+", choices=list(WORKLOADS),
                        default=list(WORKLOADS))
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=False)
    engines = {"otter": str(args.otter.resolve()),
               "node": shutil.which("node"), "bun": shutil.which("bun")}
    metadata = {
        "head": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        "uptime": subprocess.check_output(["uptime"], text=True).strip(),
        "uname": list(os.uname()),
        "stress": os.environ.get("OTTER_GC_STRESS"),
        "executables": {e: {"path": engines[e], "sha256": digest(engines[e]),
                            "version": subprocess.check_output([engines[e], "--version"], text=True).strip()}
                        for e in args.engines},
        "scripts": {w: {"path": WORKLOADS[w], "sha256": digest(ROOT / WORKLOADS[w])}
                    for w in args.workloads},
    }
    (args.out / "environment.json").write_text(json.dumps(metadata, indent=2) + "\n")
    rows = []
    for workload in args.workloads:
        for engine in args.engines:
            command = [engines[engine]] + (["run"] if engine == "otter" else [])
            if engine == "bun":
                # Bun otherwise parses these legacy scripts as strict modules.
                # Indirect eval preserves Script semantics and the exact bytes.
                command += ["-e", '(0, eval)(require("node:fs").readFileSync(process.argv[1], "utf8"))']
            command.append(str(ROOT / WORKLOADS[workload]))
            prefix = args.out / f"{workload}-{engine}"
            print(f"START {workload} {engine}", flush=True)
            with prefix.with_suffix(".stdout").open("w") as out, prefix.with_suffix(".time").open("w") as err:
                result = subprocess.run(["/usr/bin/time", "-l", *command], cwd=ROOT,
                                        stdout=out, stderr=err)
            counters = prefix.with_suffix(".time").read_text()
            row = {"workload": workload, "engine": engine, "command": command,
                   "exit": result.returncode}
            for label, key in [("instructions retired", "instructions"),
                               ("maximum resident set size", "rss_bytes")]:
                match = re.search(r"^\s*(\d+)\s+" + label + r"\s*$", counters, re.M)
                row[key] = int(match[1]) if match else None
            rows.append(row)
            (args.out / "results.json").write_text(json.dumps(rows, indent=2) + "\n")
            print(json.dumps(row), flush=True)
            if result.returncode or row["instructions"] is None:
                raise SystemExit(f"Incomplete measurement: {prefix}")


if __name__ == "__main__":
    main()
