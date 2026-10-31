#!/usr/bin/env python3
"""Where the isolate thread's time goes by tier, and what the generated code's
own instructions are doing, from one `sample` report, the run's
`--perf-basic-prof` map and its `--jit-artifacts` directory.

usage: jit_anatomy.py <sample.txt> <perf.map> <jit-artifacts-dir> [top-fns]

Sections:
1. Tier residency: every isolate-thread sample, by the tier of the nearest
   generated-code frame on its stack (`graph`, `template`) and whether the
   leaf is that code itself (`self`) or Rust it called (`-> rust`). Stacks
   with no generated frame are `no jit` (interpreter, compiler, GC, startup).
2. Buckets: every self sample in generated code, classified by the
   instruction at the sampled pc and the graph node (or template op) whose
   asm region holds it:
     prologue   entry region / before the first node region
     slow       out-of-line region (deferred paths, exits, epilogue)
     shape      CheckShapes/CheckMaps-style receiver guards
     guard      every other Check* node (elements, bounds, deps, values)
     convert    Checked*/To*/Tagged*/*ToTagged representation changes
     poll       backedge fuel, interrupt and stack polls
     call       Call*/Construct*/Invoke* nodes (arg setup, the call itself)
     spill      loads/stores against the frame (fp/sp) outside prologue/call
     move       register-to-register moves (phi resolution, copies)
     barrier    WriteBarrier nodes
     alloc      New*/Allocate*/Create* nodes (inline allocation, init stores)
     branch     Branch/Jump/Return/Switch (truthiness and compare-branch)
     ic         generic, dictionary and keyed property nodes, builtin dispatch
     field      known-shape field/slot/element loads and stores
     work       everything else (arithmetic, compares, strings, ...)
   Template code is bucketed by op kind instead (its register file is the
   frame, so every op's frame traffic is structural, not spill).
3. Top functions by self samples with their bucket split and static
   instruction count of the in-line body (node regions only).
"""
import bisect, collections, glob, os, re, sys

sample, perf_map, artifacts = sys.argv[1:4]
top = int(sys.argv[4]) if len(sys.argv) > 4 else 12

text = open(sample).read()
graph = text.split("Call graph:", 1)[1].split("Total number in stack", 1)[0]
thread = graph.split("otter-isolate", 1)[1].split("\n    ", 1)[1]
thread = re.split(r"\n    \d+ Thread_", thread, maxsplit=1)[0]
entries = []
for line in thread.splitlines():
    m = re.match(r"^([\s+!:|]*?)(\d+) (.*)$", line)
    if m:
        entries.append((len(m.group(1)), int(m.group(2)), m.group(3)))


def pc_of(name):
    m = re.search(r"\?\?\?.*\[0x([0-9a-f]+)\]", name)
    return int(m.group(1), 16) if m else None


codes = []
for line in open(perf_map):
    start, size, name = line.split(" ", 2)
    m = re.search(r"\bc(\d+)\]", name)
    codes.append((int(start, 16), int(size, 16), name.strip(), m and m.group(1)))
codes.sort()
starts = [c[0] for c in codes]


def code_of(pc):
    i = bisect.bisect_right(starts, pc) - 1
    if i < 0 or pc >= codes[i][0] + codes[i][1]:
        return None
    return codes[i]


def tier_of(code):
    name = code[2]
    if name.startswith("routine:"):
        return "routine"
    if "optimizing" in name or "graph" in name:
        return "graph"
    if "template" in name:
        return "template"
    m = re.search(r"\[(\w+)", name)
    return m.group(1) if m else "jit"


# (leaf pc or None, leaf name, nearest jit code, own count)
leaves = []
stack = []
for i, (depth, count, name) in enumerate(entries):
    while stack and stack[-1][0] >= depth:
        stack.pop()
    stack.append((depth, name))
    children, j = 0, i + 1
    while j < len(entries) and entries[j][0] > depth:
        if entries[j][0] == entries[i + 1][0]:
            children += entries[j][1]
        j += 1
    own = count - children
    if own <= 0:
        continue
    pc = pc_of(name)
    leaf_code = code_of(pc) if pc is not None else None
    caller = None
    caller_pc = None
    for _, n in reversed(stack[:-1]):
        p = pc_of(n)
        if p is not None:
            c = code_of(p)
            if c is not None:
                caller, caller_pc = c, p
                break
    leaves.append((pc, name, leaf_code, caller, caller_pc, own))

total = sum(l[-1] for l in leaves)
residency = collections.Counter()
for pc, name, leaf_code, caller, _, own in leaves:
    if leaf_code is not None:
        residency[f"{tier_of(leaf_code)} self"] += own
    elif pc is not None and caller is not None:
        residency[f"{tier_of(caller)} shared routine"] += own
    elif caller is not None:
        residency[f"{tier_of(caller)} -> rust"] += own
    else:
        residency["no jit"] += own
print(f"isolate samples: {total}")
print("== tier residency")
for k, v in residency.most_common():
    print(f"{v:7d} {100.0 * v / max(total, 1):5.1f}%  {k}")

# ---- asm parsing per code object
asm_cache = {}


def asm_of(code_id):
    if code_id in asm_cache:
        return asm_cache[code_id]
    regions, insns = [], {}
    for path in glob.glob(os.path.join(artifacts, f"*-c{code_id}", "asm.txt")):
        for line in open(path):
            m = re.search(r"region kind=([\w-]+) range=\+0x([0-9a-f]+)\.\.\+0x([0-9a-f]+)(.*)", line)
            if m:
                op = re.search(r'tier-op="(v\d+ )?(\w+)', m.group(4))
                label = op.group(2) if op else f"[{m.group(1)}]"
                kind = re.search(r'tier-op="[^"]*?(?:kind: |dictionary: )(\w+)', m.group(4))
                detail = f"{label}.{kind.group(1)}" if kind else label
                regions.append((int(m.group(2), 16), int(m.group(3), 16), m.group(1), detail))
                continue
            m = re.match(r"^\+0x([0-9a-f]+): (?:[0-9a-f]{8}  )?(.*)$", line)
            if m:
                insns[int(m.group(1), 16)] = m.group(2).strip()
    regions.sort()
    asm_cache[code_id] = (regions, insns)
    return asm_cache[code_id]


SPILL = re.compile(r"^(ldr|str|ldur|stur|ldp|stp|ldrb|strb|ldrh|strh)\b.*\[(x29|sp)\b")
MOVE = re.compile(r"^(mov|fmov)\s+[xwd]\d+,\s*[xwd]\d+$")


def node_bucket(label):
    label = label.split(".", 1)[0]
    if label in ("CheckShapes", "CheckMaps", "CheckShape", "CheckReceiverShape"):
        return "shape"
    if label.startswith("Checked") or "ToTagged" in label or label.startswith("Tagged") \
            or label.startswith("Unbox") or label.startswith("Box") or "ToFloat64" in label \
            or "ToInt32" in label or label.startswith("To"):
        return "convert"
    if label.startswith("Check") or label.startswith("Guard") or label.startswith("Assert"):
        return "guard"
    if "Backedge" in label or "Interrupt" in label or "StackCheck" in label or label == "JumpLoop":
        return "poll"
    if label.startswith("Call") or label.startswith("Construct") or label.startswith("Invoke") \
            or label.startswith("TailCall"):
        return "call"
    if label == "WriteBarrier":
        return "barrier"
    if label.startswith("New") or label.startswith("Allocate") or label.startswith("Create"):
        return "alloc"
    if label in ("Branch", "Jump", "Return", "Switch"):
        return "branch"
    if label in ("LoadNamedProperty", "StoreNamedProperty", "Generic", "LoadKeyedCached",
                 "StoreKeyedCached", "LoadKeyed", "StoreKeyed", "BuiltinMethod", "LoadGlobal",
                 "StoreGlobal", "HasProperty", "InstanceOf"):
        return "ic"
    if label.startswith("LoadOwn") or label.startswith("StoreOwn") or label.startswith("LoadShaped") \
            or label in ("LoadField", "StoreField", "LoadCellValue", "LoadThis", "LoadContextSlot",
                         "StoreContextSlot", "LoadElement", "StoreElement", "LoadArrayLength"):
        return "field"
    return None


def classify(code, pc):
    regions, insns = asm_of(code[3])
    off = pc - code[0]
    insn = insns.get(off, "")
    tier = tier_of(code)
    region = None
    for lo, hi, kind, label in regions:
        if lo <= off < hi:
            region = (kind, label)
    if region is None:
        if regions and off < regions[0][0]:
            return tier, "prologue", "(prologue)", insn
        node_hi = max((hi for lo, hi, k, _ in regions if k == "instruction"), default=0)
        if regions and off >= node_hi:
            return tier, "slow", "(out of line)", insn
        if MOVE.match(insn):
            return tier, "move", "(between nodes)", insn
        if SPILL.match(insn):
            return tier, "spill", "(between nodes)", insn
        return tier, "work", "(between nodes)", insn
    kind, label = region
    if kind in ("entryPrologue", "prologue", "osr-entry"):
        return tier, "prologue", label, insn
    if kind == "out-of-line":
        return tier, "slow", label, insn
    if tier == "template":
        return tier, "tmpl:" + label, label, insn
    b = node_bucket(label)
    if b in ("call", "prologue"):
        return tier, b, label, insn
    if SPILL.match(insn):
        return tier, "spill", label, insn
    if MOVE.match(insn):
        return tier, "move", label, insn
    return tier, b or "work", label, insn


buckets = {"graph": collections.Counter(), "template": collections.Counter()}
per_fn = collections.defaultdict(collections.Counter)
per_node = collections.Counter()
routine_by_caller = collections.Counter()
gen_total = 0
for pc, name, leaf_code, caller, caller_pc, own in leaves:
    if leaf_code is not None:
        tier, bucket, label, insn = classify(leaf_code, pc)
        buckets.setdefault(tier, collections.Counter())[bucket] += own
        per_fn[leaf_code[2]][bucket] += own
        per_node[(tier, label)] += own
        gen_total += own
    elif pc is not None and caller is not None:
        tier, bucket, label, _ = classify(caller, caller_pc - 4)
        routine_by_caller[(tier_of(caller), label)] += own

for tier, c in buckets.items():
    t = sum(c.values())
    if not t:
        continue
    print(f"== {tier} self buckets ({t} samples, {100.0 * t / max(total, 1):.1f}% of isolate)")
    for k, v in c.most_common(25):
        print(f"{v:7d} {100.0 * v / t:5.1f}%  {k}")
print("== hottest nodes/ops (generated self)")
for (tier, label), v in per_node.most_common(25):
    print(f"{v:7d} {100.0 * v / max(gen_total, 1):5.1f}%  {tier} {label}")
rt = sum(routine_by_caller.values())
if rt:
    print(f"== shared routines by calling node ({rt} samples)")
    for (tier, label), v in routine_by_caller.most_common(15):
        print(f"{v:7d} {100.0 * v / rt:5.1f}%  {tier} {label}")
print("== top functions (self samples, bucket split, static in-line instrs)")
for fn, c in sorted(per_fn.items(), key=lambda kv: -sum(kv[1].values()))[:top]:
    t = sum(c.values())
    m = re.search(r"\bc(\d+)\]", fn)
    static = 0
    if m:
        regions, insns = asm_of(m.group(1))
        lines = [o for o in insns if any(lo <= o < hi for lo, hi, k, _ in regions if k == "instruction")]
        static = len(lines)
    split = " ".join(f"{k}:{100.0 * v / t:.0f}" for k, v in c.most_common(6))
    print(f"{t:6d} {100.0 * t / max(gen_total, 1):5.1f}%  [{static:5d} insn] {fn[:70]}  {split}")
print("== hottest instructions (generated self)")
per_insn = collections.Counter()
for pc, name, leaf_code, caller, caller_pc, own in leaves:
    if leaf_code is not None:
        tier, bucket, label, insn = classify(leaf_code, pc)
        per_insn[(leaf_code[3], pc - leaf_code[0], label, bucket, insn)] += own
for (cid, off, label, bucket, insn), v in per_insn.most_common(int(os.environ.get("ANATOMY_INSNS", "40"))):
    print(f"{v:6d} {100.0 * v / max(gen_total, 1):5.1f}%  c{cid}+{off:#x} {label:24.24} {bucket:9} {insn}")
