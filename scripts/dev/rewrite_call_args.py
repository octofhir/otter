#!/usr/bin/env python3
"""Rewrite calls `<callee>(args...)` that have exactly <n> top-level
arguments: insert <arg> at position <index> and append <suffix> after the
closing parenthesis. Balanced parentheses, brackets, braces and string
literals are respected; only files under the repository are touched.

usage: rewrite_call_args.py <callee> <n> <index> <arg> <suffix> <file>...
"""
import sys

callee, n, index, arg, suffix = sys.argv[1], int(sys.argv[2]), int(sys.argv[3]), sys.argv[4], sys.argv[5]
for path in sys.argv[6:]:
    src = open(path).read()
    out, pos, changed = [], 0, 0
    while True:
        at = src.find(callee + "(", pos)
        if at < 0:
            out.append(src[pos:])
            break
        start = at + len(callee) + 1
        depth, i, commas, in_str = 0, start, [], False
        while True:
            c = src[i]
            if in_str:
                if c == "\\":
                    i += 1
                elif c == '"':
                    in_str = False
            elif c == '"':
                in_str = True
            elif c in "([{":
                depth += 1
            elif c in ")]}":
                if depth == 0:
                    break
                depth -= 1
            elif c == "," and depth == 0:
                commas.append(i)
            i += 1
        end = i
        body = src[start:end]
        parts, last = [], start
        for comma in commas:
            parts.append(src[last:comma])
            last = comma + 1
        parts.append(src[last:end])
        if parts and parts[-1].strip() == "":
            parts.pop()
        if len(parts) == n:
            parts = [p.strip() for p in parts]
            parts.insert(index, arg)
            out.append(src[pos:start] + ", ".join(parts) + ")" + suffix)
            pos = end + 1
            changed += 1
        else:
            out.append(src[pos:end + 1])
            pos = end + 1
    if changed:
        open(path, "w").write("".join(out))
        print(f"{path}: {changed}")
