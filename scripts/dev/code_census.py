#!/usr/bin/env python3
"""Generated-code bytes by graph operation kind across a --jit-artifacts run.

usage: code_census.py <artifact-dir> [top]
Sums, over every optimizing bundle, the bytes of each `asm.txt` region by
the kind of its graph operation (`CallJs`, `StoreNamedProperty`, ...), plus
structural regions (out-of-line code, prologue), and prints the totals with
their share and the number of operations of each kind.
"""
import collections, glob, re, sys

root = sys.argv[1]
top = int(sys.argv[2]) if len(sys.argv) > 2 else 40
bytes_by_kind, count_by_kind = collections.Counter(), collections.Counter()
total_code, bundles = 0, 0
for asm in glob.glob(f"{root}/*optimizing*/asm.txt"):
    bundles += 1
    for line in open(asm):
        if line.startswith("; code-bytes="):
            total_code += int(line.split("=")[1])
        m = re.search(r"region kind=(\S+) range=\+0x([0-9a-f]+)\.\.\+0x([0-9a-f]+)", line)
        if not m:
            continue
        size = int(m.group(3), 16) - int(m.group(2), 16)
        op = re.search(r'tier-op="v\d+ ([A-Za-z0-9]+)', line)
        kind = op.group(1) if op else f"<{m.group(1)}>"
        bytes_by_kind[kind] += size
        count_by_kind[kind] += 1
print(f"{bundles} optimizing bundles, {total_code} code bytes")
for kind, size in bytes_by_kind.most_common(top):
    print(f"{size:10d} {100 * size / max(total_code, 1):5.1f}%  {count_by_kind[kind]:8d}  {kind}")
