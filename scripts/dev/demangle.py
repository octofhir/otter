#!/usr/bin/env python3
"""Filter: shorten Rust v0 symbols on stdin to their last identifier path.

usage: <profile output> | scripts/dev/demangle.py
"""
import re
import sys


def readable(name):
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


for line in sys.stdin:
    print(re.sub(r'_R[A-Za-z0-9_$]+', lambda m: readable(m.group(0)), line.rstrip('\n')))
