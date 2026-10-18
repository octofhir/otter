#!/usr/bin/env python3
"""Make a GC body a value cell: insert `map: TypeMap<TAG>` as its first field.

usage: add_map_field.py <file.rs> <Struct> <TAG_CONST> [--repr-c]
Adds `value_cell` to the struct's `#[pelt(tag = ...)]` attribute when it has
one, and `#[repr(C)]` with --repr-c when the struct has no repr.
"""
import re
import sys

path, name, tag = sys.argv[1:4]
repr_c = "--repr-c" in sys.argv
src = open(path).read()
m = re.search(r"\npub(?:\([a-z]+\))? struct " + name + r"\b[^{;]*\{\n", src)
assert m, f"struct {name} not found"
field = (f"    /// Map word: this value cell's type, at body offset 0.\n"
         f"    pub(crate) map: otter_gc::cell_map::TypeMap<{tag}>,\n")
src = src[:m.end()] + field + src[m.end():]
# Attributes directly above the struct (contiguous #[...] / /// lines).
start = m.start() + 1
lines_before = src[:start].split("\n")
i = len(lines_before) - 2
while i >= 0 and (lines_before[i].startswith("#[") or lines_before[i].startswith("///")
                  or lines_before[i].startswith(")]") or lines_before[i].startswith("    ")
                  and lines_before[i - 1].startswith("#[")):
    i -= 1
attr_block = "\n".join(lines_before[i + 1:-1])
new_block = attr_block
if "#[pelt(" in new_block and "value_cell" not in new_block:
    new_block = re.sub(r"#\[pelt\(tag = ([A-Za-z0-9_:]+)", r"#[pelt(tag = \1, value_cell", new_block, count=1)
if repr_c and "#[repr(" not in new_block:
    new_block = new_block + ("\n" if new_block else "") + "#[repr(C)]"
head = "\n".join(lines_before[:i + 1])
src = head + ("\n" if head else "") + new_block + ("\n" if new_block else "") + src[start:]
open(path, "w").write(src)
print(f"{name}: ok")
