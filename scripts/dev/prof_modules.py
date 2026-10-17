#!/usr/bin/env python3
"""Self samples grouped by crate::module of the leaf frame.

usage: prof_modules.py tp.xml [CRATE_REGEX] [N]
Leaf frames are Rust v0-mangled names; the crate and first module segment
name the bucket (e.g. otter_vm::object_internal_ops).
"""
import collections, re, sys
import xml.etree.ElementTree as ET

path = sys.argv[1]
crate = re.compile(sys.argv[2] if len(sys.argv) > 2 else r"otter_")
n = int(sys.argv[3]) if len(sys.argv) > 3 else 40

def idents(name):
    """Length-prefixed identifiers of a v0-mangled symbol, in order."""
    out, i = [], 0
    while i < len(name):
        if name[i].isdigit() and (i == 0 or not name[i - 1].isdigit()):
            j = i
            while j < len(name) and name[j].isdigit():
                j += 1
            length = int(name[i:j])
            if j < len(name) and name[j] == "_":
                j += 1
            ident = name[j : j + length]
            if ident and (ident[0].isalpha() or ident[0] == "_"):
                out.append(ident)
                i = j + length
                continue
            i = j
        else:
            i += 1
    return out

def bucket(name):
    words = idents(name)
    for i, w in enumerate(words):
        if crate.fullmatch(w):
            return "::".join(words[i : i + 2])
    return None

frames, stacks = {}, {}
count = collections.Counter(); total = 0
for _, el in ET.iterparse(path, events=("end",)):
    if el.tag == "frame" and "id" in el.attrib:
        frames[el.attrib["id"]] = el.attrib.get("name", "?")
    elif el.tag == "backtrace" and "id" in el.attrib:
        stacks[el.attrib["id"]] = [frames.get(f.attrib.get("id") or f.attrib.get("ref"), "?") for f in el.findall("frame")]
    elif el.tag == "row":
        bt = el.find("tagged-backtrace/backtrace")
        names = stacks.get(bt.attrib.get("id") or bt.attrib.get("ref"), []) if bt is not None else []
        el.clear()
        total += 1
        if not names:
            continue
        # The first frame (leaf upward) that belongs to a matching crate.
        for name in names:
            b = bucket(name)
            if b:
                count[b] += 1
                break
print("samples", total)
for k, v in count.most_common(n):
    print(f"{v:6d} {100*v/total:5.1f}%  {k}")
