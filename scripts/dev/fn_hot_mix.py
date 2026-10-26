#!/usr/bin/env python3
"""Self samples of optimized code by instruction class, from a
`sample_kind_prof.py` output directory and that run's perf map: how much of
the sampled generated-code time sits on stack loads/stores, other memory,
branches and arithmetic.

usage: fn_hot_mix.py <out-dir> <perf-map> [function-substring]
"""
import bisect, collections, glob, os, re, sys

out, perf = sys.argv[1], sys.argv[2]
want = sys.argv[3] if len(sys.argv) > 3 else ""
maps = []
for line in open(perf):
    a, n, name = line.split(" ", 2)
    maps.append((int(a, 16), int(n, 16), name.strip()))
maps.sort()
starts = [m[0] for m in maps]
insn_cache = {}

def insns(code_id):
    if code_id not in insn_cache:
        table = {}
        for path in glob.glob(os.path.join(out, "art", f"*-c{code_id}")):
            for line in open(os.path.join(path, "asm.txt")):
                m = re.match(r"\+0x([0-9a-f]+): [0-9a-f]+\s+(.*)$", line.rstrip())
                if m:
                    table[int(m.group(1), 16)] = m.group(2)
        insn_cache[code_id] = table
    return insn_cache[code_id]

def klass(text):
    op = text.split()[0] if text else "?"
    if re.search(r"\[(sp|x29)", text):
        return "stack mem"
    if op.startswith(("ldr", "str", "ldp", "stp", "ldur", "stur")):
        return "heap mem"
    if op.startswith(("b", "cb", "tb", "ret")) and op not in ("bic", "bfi", "bfxil"):
        return "branch"
    if op in ("cmp", "cmn", "tst"):
        return "compare"
    if op.startswith("mov"):
        return "move"
    return "alu/other"

text = open(os.path.join(out, "sample.txt")).read()
graph = text.split("Call graph:", 1)[1].split("Total number in stack", 1)[0]
thread = next(t for t in re.split(r"\n    (?=\d+ Thread_)", graph) if "otter-isolate" in t.split("\n", 1)[0])
rows = []
for line in thread.splitlines():
    m = re.match(r"^([\s+!:|]*)(\d+) (.*)$", line)
    if m:
        rows.append((len(m.group(1)), int(m.group(2)), m.group(3)))
by_class = collections.Counter()
total = 0.0
for i, (depth, count, frame) in enumerate(rows):
    children, j, first = 0, i + 1, None
    while j < len(rows) and rows[j][0] > depth:
        if first is None:
            first = rows[j][0]
        if rows[j][0] == first:
            children += rows[j][1]
        j += 1
    own = count - children
    if own <= 0 or "???" not in frame or "[" not in frame:
        continue
    addresses = [int(a, 16) for a in re.findall(r"0x([0-9a-f]+)", frame.split("[", 1)[1])]
    share = own / len(addresses)
    for address in addresses:
        k = bisect.bisect_right(starts, address) - 1
        if k < 0 or address >= maps[k][0] + maps[k][1]:
            continue
        m = re.search(r"\[optimizing c(\d+)\]", maps[k][2])
        if not m or want not in maps[k][2]:
            continue
        offset = (address - maps[k][0]) & ~3
        by_class[klass(insns(m.group(1)).get(offset, ""))] += share
        total += share
print(f"optimized self samples {total:.0f}")
for name, n in by_class.most_common():
    print(f"{n:8.0f} {100 * n / max(total, 1):5.1f}%  {name}")
