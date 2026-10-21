#!/usr/bin/env python3
"""Remove `Pattern => { ... }` match arms whose pattern line starts with a
given prefix.

usage: remove_match_arms.py <file.rs> <arm-prefix>...
Each prefix names the start of the arm's pattern as written (for example
`Kind::LoadNewTarget =>`); the arm runs to its matching closing brace.
"""
import sys

path, prefixes = sys.argv[1], sys.argv[2:]
lines = open(path).read().split("\n")
for prefix in prefixes:
    start = next((i for i, line in enumerate(lines) if line.strip().startswith(prefix)), None)
    if start is None:
        sys.exit(f"{prefix}: not found in {path}")
    depth = 0
    end = start
    seen = False
    for i in range(start, len(lines)):
        for ch in lines[i]:
            if ch == "{":
                depth += 1
                seen = True
            elif ch == "}":
                depth -= 1
        if seen and depth == 0:
            end = i
            break
    del lines[start:end + 1]
open(path, "w").write("\n".join(lines))
