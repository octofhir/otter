#!/usr/bin/env python3
"""Compare `otter --interpreter` with normal tiering on the difftest corpus.

A fast first pass while iterating on a tier: no GC-stress modes, one process
per mode and file, a short timeout. Prints one line per differing file.
"""
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
otter = sys.argv[1] if len(sys.argv) > 1 else str(ROOT / "target/release/otter")
only = sys.argv[2:]
corpus = sorted((ROOT / "crates/otter-difftest/corpus").glob("*.js"))
failed = 0
for path in corpus:
    if only and not any(name in path.name for name in only):
        continue
    source = path.read_text()
    runs = []
    for mode in (["--interpreter"], []):
        try:
            result = subprocess.run([otter, "--timeout", "0", *mode, "-p", source],
                                    capture_output=True, text=True, timeout=30)
            runs.append((result.returncode, result.stdout, result.stderr))
        except subprocess.TimeoutExpired:
            runs.append(("timeout", "", ""))
    if runs[0] != runs[1]:
        failed += 1
        oracle, tiered = runs
        detail = tiered[2].strip().splitlines()[-1:] if tiered[2] else []
        print(f"DIFF {path.name}: oracle={oracle[0]} tiered={tiered[0]} {detail}", flush=True)
print(f"done: {failed} differing of {len(corpus) if not only else 'selected'}")
