#!/usr/bin/env python3
"""Self-time per symbol for the busiest thread of a macOS `sample` report.

usage: scripts/dev/sample_self.py <sample.txt> [top]
"""
import re
import sys


def readable(name):
    """Crude Rust v0 demangling: the identifier path of an `_R` symbol."""
    if not name.startswith('_R'):
        return name
    idents, i = [], 0
    while i < len(name):
        m = re.match(r'(\d+)_?', name[i:])
        if not m:
            i += 1
            continue
        n = int(m.group(1))
        start = i + len(m.group(0))
        ident = name[start:start + n]
        if n and len(ident) == n and re.fullmatch(r'[A-Za-z_][A-Za-z0-9_]*', ident):
            idents.append(ident)
            i = start + n
        else:
            i += len(m.group(1))
    return '::'.join(idents[-3:]) or name

lines = open(sys.argv[1]).read().split('\n')
top = int(sys.argv[2]) if len(sys.argv) > 2 else 30
start = next(i for i, l in enumerate(lines) if l.startswith('Call graph:'))
end = next(i for i, l in enumerate(lines) if l.startswith('Total number in stack'))
entries = []
for l in lines[start + 1:end]:
    m = re.match(r'^(\s*[+!:| ]*)(\d+) (.*)$', l)
    if not m:
        continue
    name = re.sub(r'\s+\(in [^)]*\).*', '', m.group(3))
    name = re.sub(r'\s+\+ \d+.*', '', name)
    entries.append((len(m.group(1)), int(m.group(2)), readable(name)))
# Threads are the shallowest entries; keep the isolate thread.
thread_depth = min(d for d, _, _ in entries)
threads = [i for i, e in enumerate(entries) if e[0] == thread_depth]
named = [i for i in threads if 'isolate' in entries[i][2]]
best = named[0] if named else max(threads, key=lambda i: entries[i][1])
stop = next((i for i in threads if i > best), len(entries))
body = entries[best:stop]
selfc = {}
for i, (d, c, n) in enumerate(body):
    child, j, childd = 0, i + 1, None
    while j < len(body) and body[j][0] > d:
        if childd is None:
            childd = body[j][0]
        if body[j][0] == childd:
            child += body[j][1]
        j += 1
    selfc[n] = selfc.get(n, 0) + c - child
total = sum(selfc.values())
print(f"thread: {body[0][2][:80]}  samples={total}")
for n, c in sorted(selfc.items(), key=lambda x: -x[1])[:top]:
    print(f"{100 * c / total:5.1f}% {c:6d} {n[:130]}")
