#!/usr/bin/env python3
"""Aggregate an exported xctrace time-profile table for one process name.

Usage: xctrace_top.py <profile.xml> <process-name> [--top N] [--inclusive]

Export the table first:
  xcrun xctrace export --input X.trace \
    --xpath '/trace-toc/run[@number="1"]/data/table[@schema="time-profile"]' > X.xml

Prints self (leaf) weight per function and per binary; with --inclusive
also the inclusive weight per function (counted once per sample); with
--under SUBSTR the weight of each direct callee of the outermost frame whose
name contains SUBSTR.
"""
import sys
import xml.etree.ElementTree as ET
from collections import Counter


def main():
    path, proc_name = sys.argv[1], sys.argv[2]
    top = 40
    if "--top" in sys.argv:
        top = int(sys.argv[sys.argv.index("--top") + 1])
    inclusive = "--inclusive" in sys.argv
    under = sys.argv[sys.argv.index("--under") + 1] if "--under" in sys.argv else None
    callees = Counter()
    by_id = {}
    text = open(path, encoding="utf-8", errors="replace").read()
    # xctrace may append a diagnostic line after the document.
    text = text[: text.rindex("</trace-query-result>") + len("</trace-query-result>")]
    root = ET.fromstring(text)
    # First pass: index every element with an id.
    for el in root.iter():
        i = el.get("id")
        if i is not None:
            by_id[i] = el

    def deref(el):
        r = el.get("ref")
        return by_id[r] if r is not None else el

    selfw = Counter()
    selfbin = Counter()
    incl = Counter()
    total = 0
    for row in root.iter("row"):
        proc = row.find("process")
        if proc is None:
            continue
        proc = deref(proc)
        fmt = proc.get("fmt", "")
        if not fmt.startswith(proc_name + " ") and fmt != proc_name:
            continue
        weight_el = row.find("weight")
        weight = int(deref(weight_el).text) if weight_el is not None else 1
        bt = row.find("tagged-backtrace")
        if bt is None:
            bt = row.find("backtrace")
        if bt is None:
            continue
        bt = deref(bt)
        inner = bt.find("backtrace")
        if inner is not None:
            bt = deref(inner)
        frames = []
        for fr in bt.findall("frame"):
            fr = deref(fr)
            b = fr.find("binary")
            bname = deref(b).get("name") if b is not None else "?"
            frames.append((fr.get("name") or fr.get("addr"), bname))
        if not frames:
            continue
        total += weight
        selfw[frames[0][0]] += weight
        selfbin[frames[0][1]] += weight
        if under is not None:
            # Frames run leaf-first; find the outermost match.
            for index in range(len(frames) - 1, -1, -1):
                if under in frames[index][0]:
                    callee = frames[index - 1][0] if index > 0 else "<self>"
                    callees[callee] += weight
                    break
        if inclusive:
            for name in {f[0] for f in frames}:
                incl[name] += weight
    print(f"total {total / 1e6:.1f} ms")
    print("== self by binary")
    for name, w in selfbin.most_common(15):
        print(f"{w / total * 100:6.2f}%  {name}")
    print("== self by function")
    for name, w in selfw.most_common(top):
        print(f"{w / total * 100:6.2f}%  {name}")
    if under is not None:
        print(f"== under {under}")
        for name, w in callees.most_common(top):
            print(f"{w / total * 100:6.2f}%  {name}")
    if inclusive:
        print("== inclusive")
        for name, w in incl.most_common(top):
            print(f"{w / total * 100:6.2f}%  {name}")


if __name__ == "__main__":
    main()
