#!/usr/bin/env python3
"""Rewrite `Name { a: x, b: y, ... }` struct literals into `Name::ctor(x, y, ...)`.

Usage: rewrite_struct_literal.py <Name> <ctor> <field,field,...> <file>...
Literals that already name every listed field (and no other) are rewritten;
others are left untouched and reported.
"""
import re
import sys

name, ctor, fields = sys.argv[1], sys.argv[2], sys.argv[3].split(",")


def split_top(body):
    parts, depth, start, i = [], 0, 0, 0
    while i < len(body):
        c = body[i]
        if c in "([{":
            depth += 1
        elif c in ")]}":
            depth -= 1
        elif c == "," and depth == 0:
            parts.append(body[start:i])
            start = i + 1
        elif c == '"':
            i += 1
            while body[i] != '"':
                i += 2 if body[i] == "\\" else 1
        i += 1
    if body[start:].strip():
        parts.append(body[start:])
    return [p.strip() for p in parts if p.strip()]


for path in sys.argv[4:]:
    text = open(path).read()
    out, pos, changed = [], 0, 0
    pattern = re.compile(r"(?<![A-Za-z0-9_:])((?:[A-Za-z_][A-Za-z0-9_]*::)*" + re.escape(name) + r")\s*\{")
    while True:
        m = pattern.search(text, pos)
        if not m:
            out.append(text[pos:])
            break
        start = m.end()
        depth, i = 1, start
        while depth:
            if text[i] == "{":
                depth += 1
            elif text[i] == "}":
                depth -= 1
            i += 1
        body = text[start:i - 1]
        parts = split_top(body)
        values = {}
        ok = True
        for part in parts:
            key, sep, value = part.partition(":")
            if not sep or key.strip() not in fields:
                ok = False
                break
            values[key.strip()] = value.strip()
        if ok and set(values) == set(fields) and not text[:m.start()].rstrip().endswith("struct"):
            out.append(text[pos:m.start()])
            out.append(f"{m.group(1)}::{ctor}(" + ", ".join(values[f] for f in fields) + ")")
            changed += 1
        else:
            out.append(text[pos:i])
            if not text[:m.start()].rstrip().endswith("struct") and "register_count" not in body:
                print(f"{path}: left literal at offset {m.start()}")
        pos = i
    open(path, "w").write("".join(out))
    print(f"{path}: rewrote {changed}")
