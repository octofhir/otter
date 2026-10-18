#!/usr/bin/env python3
"""Move named fields (with their docs and attributes) right after `map`.

usage: move_fields_after_map.py <file.rs> <Struct> <field> [field...]
Fields land in the given order, filling the padding beside the map word.
"""
import sys

path, name, fields = sys.argv[1], sys.argv[2], sys.argv[3:]
src = open(path).read()
i = src.index(f"struct {name} {{")
j = src.index("\n}\n", i)
lines = src[i:j].split("\n")
moved = []
for field in fields:
    k = next(n for n, l in enumerate(lines) if l.strip().split(":")[0].split()[-1] == field
             and ":" in l and not l.strip().startswith("///"))
    a = k
    while lines[a - 1].strip().startswith(("///", "#[")):
        a -= 1
    moved += lines[a:k + 1]
    del lines[a:k + 1]
m = next(n for n, l in enumerate(lines) if "map: otter_gc::cell_map::TypeMap" in l)
lines[m + 1:m + 1] = moved
open(path, "w").write(src[:i] + "\n".join(lines) + src[j:])
print(f"{name}: moved {', '.join(fields)}")
