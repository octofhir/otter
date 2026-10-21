#!/usr/bin/env python3
"""Remove top-level Rust items (fn/struct/enum/impl-free items) by name.

usage: remove_rust_items.py <file.rs> <item-name>...
Deletes each named `fn`/`const`/`struct` definition together with the doc
comments and attributes directly above it, matching braces to find its end.
Used to drop code a deleted tier left without callers.
"""
import re
import sys

path, names = sys.argv[1], sys.argv[2:]
src = open(path).read()
for name in names:
    pattern = re.compile(
        r"(?m)^([ \t]*)(?:pub(?:\([^)]*\))?\s+)?(?:const\s+fn|fn|const|struct|enum)\s+" + re.escape(name) + r"\b"
    )
    match = pattern.search(src)
    if not match:
        sys.exit(f"{name}: not found in {path}")
    start = match.start()
    # Walk back over doc comments and attributes.
    lines_before = src[:start].split("\n")
    keep = len(lines_before) - 1
    while keep > 0:
        line = lines_before[keep - 1].strip()
        if line.startswith("///") or line.startswith("#[") or line.startswith("#!["):
            keep -= 1
        else:
            break
    start = len("\n".join(lines_before[:keep])) + (1 if keep else 0)
    # Find the end: a `;` before any `{`, or the matching closing brace.
    index = match.end()
    depth = 0
    nesting = 0
    while index < len(src):
        ch = src[index]
        if ch in "([":
            nesting += 1
        elif ch in ")]":
            nesting -= 1
        elif ch == ";" and depth == 0 and nesting == 0:
            index += 1
            break
        elif ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
            if depth == 0:
                index += 1
                break
        index += 1
    # Swallow one trailing newline and a following blank line.
    if src[index:index + 1] == "\n":
        index += 1
    if src[index:index + 1] == "\n" and src[start - 1:start] == "\n":
        index += 1
    src = src[:start] + src[index:]
open(path, "w").write(src)
