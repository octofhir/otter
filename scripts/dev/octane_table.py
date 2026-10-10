#!/usr/bin/env python3
"""Summarize a `benchmarks/run-octane.sh` log as a per-suite score table.

usage: octane_table.py <octane-log> [<older-log>]

Prints each suite's node / bun / otter score, otter's share of node's score
and, with an older log, otter's change against it. Scores are higher-better;
a missing score (crash, timeout) prints as `-`.
"""
import math
import re
import sys


def parse(path):
    scores = {}
    suite = engine = None
    for line in open(path, errors="replace"):
        if line.startswith("### "):
            suite = line[4:].strip()
            scores.setdefault(suite, {})
        elif line.startswith("=== ") and line.rstrip().endswith(" ==="):
            engine = line.strip("= \n")
        elif suite and engine:
            match = re.match(r"Score \(version \d+\): (\d+)", line)
            if match:
                scores[suite][engine] = int(match.group(1))
    return scores


current = parse(sys.argv[1])
older = parse(sys.argv[2]) if len(sys.argv) > 2 else {}
header = f"{'suite':14} {'node':>8} {'bun':>8} {'otter':>8} {'otter/node':>10}"
if older:
    header += f" {'vs older':>9}"
print(header)
ratios = []
for suite, row in current.items():
    node, bun, otter = row.get("node"), row.get("bun"), row.get("otter")
    ratio = otter / node if otter and node else None
    if ratio:
        ratios.append(ratio)
    cells = [f"{v:>8}" if v else f"{'-':>8}" for v in (node, bun, otter)]
    line = f"{suite:14} {' '.join(cells)} {f'{ratio:.2f}' if ratio else '-':>10}"
    if older:
        before = older.get(suite, {}).get("otter")
        line += f" {f'{otter / before - 1:+.0%}' if otter and before else '-':>9}"
    print(line)
if ratios:
    geo = math.exp(sum(math.log(r) for r in ratios) / len(ratios))
    print(f"{'geomean':14} {'':>8} {'':>8} {'':>8} {geo:>10.2f}")
