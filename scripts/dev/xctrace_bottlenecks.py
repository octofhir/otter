#!/usr/bin/env python3
"""Cycle-weighted CPU bottleneck shares (normalized over the four
categories) per thread from an xctrace
`CPU Counters` recording's `MetricAggregationForThread` table.

usage: xctrace_bottlenecks.py <table.xml> [thread-substring]
Export the table first:
  xcrun xctrace export --input X.trace \
    --xpath '/trace-toc/run[@number="1"]/data/table[@schema="MetricAggregationForThread"]' > X.xml
"""
import collections, sys
import xml.etree.ElementTree as ET

path = sys.argv[1]
want = sys.argv[2] if len(sys.argv) > 2 else ""
ids = {}

def value(el):
    if el is None:
        return None
    if "ref" in el.attrib:
        return ids[el.attrib["ref"]]
    ids[el.attrib["id"]] = el
    for child in el:
        value(child)
    return el

cycles = collections.Counter()
weighted = collections.defaultdict(collections.Counter)
current = {}
for row in ET.parse(path).getroot().iter("row"):
    cells = [value(c) for c in row]
    start, thread, number, share, name = cells[0], cells[2], cells[4], cells[5], cells[6]
    tname = thread.attrib.get("fmt", "")
    if want and want not in tname:
        continue
    metric = name.text
    key = (tname, start.text)
    if metric == "cycle":
        current[tname] = int(number.text)
        cycles[tname] += int(number.text)
    else:
        weighted[tname][metric] += float(share.text) * current.get(tname, 0)
for tname, total in cycles.most_common(8):
    norm = sum(weighted[tname].values()) or 1
    shares = {m: 100 * v / norm for m, v in weighted[tname].items()}
    text = "  ".join(f"{m} {s:5.1f}%" for m, s in sorted(shares.items()))
    print(f"{total/1e9:6.2f}G cycles  {tname[:60]:60}  {text}")
