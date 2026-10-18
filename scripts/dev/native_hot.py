#!/usr/bin/env python3
"""Self samples of one native function by source line, inlined frames named.

usage: native_hot.py <time-profile.xml> <symbol-substring> [top]
Takes every sample whose leaf frame is the function, symbolizes its address
with `atos -i` (which names each inlined frame and its file:line), and
prints the samples per innermost source line and per inlined function.
"""
import collections, subprocess, sys
import xml.etree.ElementTree as ET

path, want = sys.argv[1], sys.argv[2]
top = int(sys.argv[3]) if len(sys.argv) > 3 else 40
frames, binaries, leaves = {}, {}, {}
addrs = collections.Counter()
binary_path = load = None
for _, el in ET.iterparse(path, events=("end",)):
    if el.tag == "binary" and "id" in el.attrib:
        binaries[el.get("id")] = (el.get("path"), el.get("load-addr"))
    if el.tag == "frame" and "id" in el.attrib:
        b = el.find("binary")
        bid = None if b is None else (b.get("id") or b.get("ref"))
        frames[el.get("id")] = (el.get("name") or "", el.get("addr"), bid)
    if el.tag == "tagged-backtrace" and "id" in el.attrib:
        first = el.find("backtrace/frame")
        if first is not None:
            leaves[el.get("id")] = first.get("id") or first.get("ref")
    if el.tag == "row":
        tagged = el.find("tagged-backtrace")
        if tagged is None:
            continue
        fid = leaves.get(tagged.get("id") or tagged.get("ref"))
        if fid is None:
            continue
        name, addr, bid = frames.get(fid, ("", None, None))
        if want in name and addr:
            addrs[addr] += 1
            if bid in binaries and binary_path is None:
                binary_path, load = binaries[bid]
        el.clear()
if not addrs:
    sys.exit("no samples")
order = list(addrs)
out = subprocess.run(["xcrun", "atos", "-o", binary_path, "-l", load, "-i", *order],
                     capture_output=True, text=True).stdout
# atos -i prints the inlined chain for each address, innermost first, blank-separated.
chains = [block.strip().splitlines() for block in out.strip().split("\n\n")]
by_line, by_fn = collections.Counter(), collections.Counter()
if len(chains) != len(order):
    # Without blank separators each address may span several lines; fall back to one.
    chains = [[line] for line in out.strip().splitlines()][: len(order)]
for addr, chain in zip(order, chains):
    n = addrs[addr]
    inner = chain[0] if chain else "?"
    by_line[inner.split(" (in ")[0][:60] + "  " + inner.rsplit("(", 1)[-1].rstrip(")")] += n
    for fn in {c.split(" (in ")[0][:90] for c in chain}:
        by_fn[fn] += n
total = sum(addrs.values())
print(f"{total} self samples in {want}")
print("== innermost source line")
for k, n in by_line.most_common(top):
    print(f"{n:6} {100*n/total:5.1f}%  {k}")
print("== inlined function (any depth)")
for k, n in by_fn.most_common(top):
    print(f"{n:6} {100*n/total:5.1f}%  {k}")
