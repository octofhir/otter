#!/usr/bin/env python3
"""Attribute samples of leaf frames (allocator, memmove...) to their owners.

usage: prof_leaf_owner.py tp.xml LEAF_REGEX [OWNER_REGEX] [N] [DEPTH]
For every sample whose leaf matches LEAF_REGEX, walk up the stack and count
the first DEPTH frames matching OWNER_REGEX (default: otter crates), joined.
"""
import collections, re, sys
import xml.etree.ElementTree as ET

path, leaf = sys.argv[1], re.compile(sys.argv[2])
owner = re.compile(sys.argv[3] if len(sys.argv) > 3 else r"otter_")
n = int(sys.argv[4]) if len(sys.argv) > 4 else 30
depth = int(sys.argv[5]) if len(sys.argv) > 5 else 1

def short(name):
    name = re.sub(r"::h[0-9a-f]{16}$", "", name)
    m = re.findall(r"\d+([A-Za-z_][A-Za-z0-9_]*)", name)
    return "::".join(m[-3:]) if m else name[:100]

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
        if not names or not leaf.search(names[0]):
            continue
        total += 1
        owners = [short(x) for x in names[1:] if owner.search(x)][:depth]
        count[" <- ".join(owners) or "(none)"] += 1
print("matching samples", total)
for k, v in count.most_common(n):
    print(f"{v:6d}  {k}")
