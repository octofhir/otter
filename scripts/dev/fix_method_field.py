#!/usr/bin/env python3
"""Turn field accesses cargo reports as E0615 (`attempted to take value of
method`) into accessor calls: reads `x.name` -> `x.name()`, assignments
`x.name = e;` -> `x.set_name(e);`, `x.name += e;` -> `x.set_name(x.name() + e);`.

usage: cargo check ... 2>&1 | fix_method_field.py <name>
"""
import collections
import re
import sys

name = sys.argv[1]
text = sys.stdin.read()
sites = collections.defaultdict(set)
for m in re.finditer(r"error\[E0615\]: attempted to take value of method `" + name +
                     r"`[^\n]*\n\s*--> ([^:]+):(\d+):(\d+)", text):
    sites[m.group(1)].add((int(m.group(2)), int(m.group(3))))
for path, locs in sites.items():
    lines = open(path).read().split("\n")
    for line, col in sorted(locs, key=lambda t: (t[0], -t[1])):
        s = lines[line - 1]
        c = col - 1
        assert s[c:c + len(name)] == name, (path, line, s)
        after = s[c + len(name):]
        # receiver expression: back from the dot to a delimiter
        dot = c - 1
        assert s[dot] == "."
        r = dot
        depth = 0
        while r > 0:
            ch = s[r - 1]
            if ch in ")]":
                depth += 1
            elif ch in "([":
                if depth == 0:
                    break
                depth -= 1
            elif depth == 0 and not (ch.isalnum() or ch in "_.*&"):
                break
            r -= 1
        recv = s[r:dot]
        am = re.match(r"\s*(\+|-)?=(?!=)\s*(.*?);(.*)$", after)
        if am:
            op, expr, rest = am.groups()
            if op:
                expr = f"{recv}.{name}() {op} {expr}"
            lines[line - 1] = s[:r] + f"{recv}.set_{name}({expr});" + rest
        else:
            lines[line - 1] = s[:c + len(name)] + "()" + after
    open(path, "w").write("\n".join(lines))
    print(path, len(locs))
