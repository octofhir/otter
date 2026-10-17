#!/usr/bin/env python3
"""Top self and inclusive frames of a Time Profiler time-profile export.

usage: prof_top.py tp.xml [N] [--filter REGEX] [--under REGEX]
--under keeps only samples with a frame matching REGEX on the stack.
Frame names are shortened: Rust hash suffixes and generic args dropped.
"""
import collections, re, sys
import xml.etree.ElementTree as ET

args = sys.argv[1:]
path = args.pop(0)
n = 40
flt = under = None
while args:
    a = args.pop(0)
    if a == "--filter": flt = re.compile(args.pop(0))
    elif a == "--under": under = re.compile(args.pop(0))
    else: n = int(a)

def short(name):
    name = re.sub(r"::h[0-9a-f]{16}$", "", name)
    name = re.sub(r"<[^<>]*>", "", name)
    name = re.sub(r"<[^<>]*>", "", name)
    return name[:140]

frames, stacks = {}, {}
self_c = collections.Counter(); incl = collections.Counter(); total = 0
for _, el in ET.iterparse(path, events=("end",)):
    if el.tag == "frame" and "id" in el.attrib:
        frames[el.attrib["id"]] = short(el.attrib.get("name", "?"))
    elif el.tag == "backtrace" and "id" in el.attrib:
        stacks[el.attrib["id"]] = [frames.get(f.attrib.get("id") or f.attrib.get("ref"), "?") for f in el.findall("frame")]
    elif el.tag == "row":
        bt = el.find("tagged-backtrace/backtrace")
        names = stacks.get(bt.attrib.get("id") or bt.attrib.get("ref"), []) if bt is not None else []
        el.clear()
        if under and not any(under.search(x) for x in names):
            continue
        total += 1
        if names:
            self_c[names[0]] += 1
        for x in set(names):
            incl[x] += 1
print("samples", total)
print("--- self")
for k, v in self_c.most_common(n):
    if flt and not flt.search(k): continue
    print(f"{v:6d} {100*v/total:5.1f}%  {k}")
print("--- inclusive")
shown = 0
for k, v in incl.most_common():
    if flt and not flt.search(k): continue
    print(f"{v:6d} {100*v/total:5.1f}%  {k}")
    shown += 1
    if shown >= n: break
