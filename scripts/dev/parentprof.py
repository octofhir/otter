#!/usr/bin/env python3
"""Inclusive samples of a frame split by its caller chain, from a macOS `sample` report.

usage: parentprof.py <sample.txt> <frame-substring> [depth] [top]
For every frame whose name contains <frame-substring>, sums its inclusive
samples per chain of the `depth` nearest callers (default 2).
"""
import collections, re, sys

text = open(sys.argv[1]).read()
needle = sys.argv[2]
depth = int(sys.argv[3]) if len(sys.argv) > 3 else 2
top = int(sys.argv[4]) if len(sys.argv) > 4 else 20
graph = text.split("Call graph:", 1)[1].split("Total number in stack", 1)[0]
stack = []
chains = collections.Counter()
for line in graph.splitlines():
    m = re.match(r"^([ +!:|]*)(\d+) (\S+)", line)
    if not m:
        continue
    indent, count, name = len(m.group(1)), int(m.group(2)), m.group(3)
    while stack and stack[-1][0] >= indent:
        stack.pop()
    if needle in name:
        chain = " <- ".join(n[:70] for _, n in reversed(stack[-depth:]))
        chains[chain] += count
    stack.append((indent, name))
for chain, count in chains.most_common(top):
    print(f"{count:7}  {chain}")
