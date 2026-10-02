#!/usr/bin/env python3
"""Disassemble a graph-tier code dump (`graph-<fid>.code`) with node marks.

Usage: graph_disasm.py graph-503.code [graph-503.offsets] [start end]
"""
import subprocess
import sys
from pathlib import Path

code = Path(sys.argv[1]).read_bytes()
marks = {}
if len(sys.argv) > 2 and Path(sys.argv[2]).exists():
    for line in Path(sys.argv[2]).read_text().split():
        offset, node = line.split(":")
        marks.setdefault(int(offset), []).append(node)
start = int(sys.argv[3]) if len(sys.argv) > 3 else 0
end = int(sys.argv[4]) if len(sys.argv) > 4 else len(code)
words = [code[i:i + 4] for i in range(start, end, 4)]
text = "\n".join(" ".join(f"0x{b:02x}" for b in w) for w in words)
out = subprocess.run(["/opt/homebrew/opt/llvm/bin/llvm-mc", "--disassemble", "-triple=aarch64"],
                     input=text, capture_output=True, text=True).stdout
lines = [l for l in out.splitlines() if l.startswith("\t")]
for index, line in enumerate(lines):
    offset = start + index * 4
    for node in marks.get(offset, []):
        print(f"      ; {node}")
    print(f"{offset:6}: {line.strip()}")
