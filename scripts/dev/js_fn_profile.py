#!/usr/bin/env python3
"""Self samples of one JS thread by tier and by JS function, for otter or
node, from a macOS `sample` report and the run's `--perf-basic-prof` map.
Functions are keyed by source line so the two engines line up.

usage: js_fn_profile.py <otter|node> <sample.txt> <perf.map> <source.js> [top] [out.json]

Idle leaves (condition waits, kevent) are dropped. `inclusive` counts a
sample once for every distinct function on its stack.

Every leaf sample is credited to the nearest JS code object on its stack
(itself when the leaf is generated code). Columns: `self` (the leaf is that
code object), `total` (self plus the runtime, builtins and shared routines it
called). Tiers: otter `optimizing`/`template`; node `*` (optimizing), `+`
(mid tier), `^` (baseline), `~` (interpreter). node's builtins and bytecode
handlers credit their JS caller like otter's routines do.
"""
import bisect, collections, re, sys

engine, sample, perf_map, source = sys.argv[1:5]
top = int(sys.argv[5]) if len(sys.argv) > 5 else 25

src = open(source, "rb").read()
line_starts = [0] + [i + 1 for i, b in enumerate(src) if b == 0x0A]


def line_of_offset(off):
    return bisect.bisect_right(line_starts, off)


text = open(sample).read()
graph = text.split("Call graph:", 1)[1].split("Total number in stack", 1)[0]
marker = "otter-isolate" if engine == "otter" else "com.apple.main-thread"
thread = graph.split(marker, 1)[1].split("\n", 1)[1]
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
for line in open(perf_map, errors="replace"):
    parts = line.rstrip("\n").split(" ", 2)
    if len(parts) == 3:
        codes.append((int(parts[0], 16), int(parts[1], 16), parts[2]))
codes.sort()
starts = [c[0] for c in codes]


def code_of(pc):
    i = bisect.bisect_right(starts, pc) - 1
    if i < 0 or pc >= codes[i][0] + codes[i][1]:
        return None
    return codes[i][2]


def js_key(name):
    """(function key, tier) for a JS code object, None for anything else."""
    if engine == "otter":
        m = re.match(r"JS:(.*)@(\d+) \[(\w+) c\d+\]", name)
        if not m:
            return None
        return f"{m.group(1)}:{line_of_offset(int(m.group(2)))}", m.group(3)
    m = re.match(r"JS:([*+^~]?)'?(\S*) (\S+):(\d+):(\d+)", name)
    if not m:
        return None
    tier = {"*": "optimizing", "+": "mid", "^": "baseline", "~": "interp", "": "?"}[m.group(1)]
    fn = m.group(2) or "<anonymous>"
    fn = fn.split(".")[-1]
    return f"{fn}:{m.group(4)}", tier


IDLE = re.compile(r"__psynch_cvwait|__semwait_signal|kevent|mach_msg2_trap|__workq_kernreturn")
stack = []
incl_by = collections.Counter()
callee_by = collections.Counter()
native_by = collections.Counter()
class_by = collections.Counter()
leaf_callee_by = collections.Counter()
native_by = collections.Counter()
class_by = collections.Counter()


def demangle_v0(name):
    """Last three path segments of a Rust v0 symbol, `::`-joined."""
    if not name.startswith("_R"):
        return name
    idents, i = [], 2
    while i < len(name):
        m = re.compile(r"(\d+)_?").match(name, i)
        if not m or m.group(1) == "0":
            i += 1
            continue
        n = int(m.group(1))
        word = name[m.end():m.end() + n]
        if len(word) == n and re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", word):
            idents.append(word)
            i = m.end() + n
        else:
            i += 1
    idents = [w for w in idents if not re.fullmatch(r"Cs[0-9A-Za-z]+|B\d+|h[0-9a-f]+", w)]
    return "::".join(idents[-3:]) or name


def short(name):
    name = re.sub(r"\s+\(in [^)]*\).*$", "", name)
    name = demangle_v0(name.strip())
    name = re.sub(r"::h[0-9a-f]{16}$", "", name)
    name = re.sub(r"\s+\+\s+\d+.*$", "", name)
    return name[:100]

self_by = collections.Counter()
total_by = collections.Counter()
tier_by = collections.Counter()
residency = collections.Counter()
total = 0
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
    if own <= 0 or IDLE.search(name):
        continue
    total += own
    on_stack = set()
    for _, n in stack:
        pc = pc_of(n)
        c = code_of(pc) if pc is not None else None
        key = js_key(c) if c is not None else None
        if key is not None:
            on_stack.add(key[0])
    for fn in on_stack:
        incl_by[fn] += own
    owner = None
    leaf_is_js = False
    for k, (_, n) in enumerate(reversed(stack)):
        pc = pc_of(n)
        if pc is None:
            continue
        c = code_of(pc)
        if c is None:
            continue
        key = js_key(c)
        if key is not None:
            owner = key
            leaf_is_js = k == 0
            break
    if owner is None:
        residency["no js frame"] += own
        continue
    fn, tier = owner
    residency[f"{tier} {'self' if leaf_is_js else '-> callee'}"] += own
    if not leaf_is_js:
        # the outermost non-JS frame above the owner names the callee
        names = [n for _, n in stack]
        idx = max(k for k, n in enumerate(names) if (lambda p: p is not None and code_of(p) is not None)(pc_of(n)))
        callee = names[idx + 1] if idx + 1 < len(names) else name
        c = code_of(pc_of(callee)) if pc_of(callee) is not None else None
        callee_by[short(c if c else callee)] += own
        cs = short(c if c else callee)
        if engine == "otter":
            cls = "native" if re.search(r"native_exit_entry|otter_native_invoke", cs) else "stub"
        else:
            cls = ("stub" if re.search(r"IC", cs) else
                   "native" if re.search(r"Builtins_|RegExp\.", cs) else "runtime")
        class_by[cls] += own
        if "native_exit_entry" in callee or "otter_native_invoke" in callee:
            body = next((short(n) for n in names[idx + 2:] if not re.search(
                r"native_exit_entry|native_invoke|call_static|NativeFunction|native_call|host_call|invoke_native|call_native", n)), short(name))
            native_by[body] += own
        leaf_callee_by[short(code_of(pc_of(name)) or name if pc_of(name) is not None else name)] += own
    total_by[fn] += own
    tier_by[(fn, tier)] += own
    if leaf_is_js:
        self_by[fn] += own

print(f"samples: {total}")
print("== residency")
for k, v in residency.most_common():
    print(f"{v:7d} {100.0 * v / max(total, 1):5.1f}%  {k}")
print("== functions (self ms, total ms, inclusive ms, tiers) at 1 ms per sample")
for fn, v in sorted(total_by.items(), key=lambda kv: -self_by[kv[0]])[:top]:
    tiers = " ".join(f"{t}:{c}" for (f, t), c in tier_by.items() if f == fn)
    print(f"{self_by[fn]:6d} {v:6d} {incl_by[fn]:6d}  {fn:40.40}  {tiers}")
print("== callees of JS code (entry frame below the JS caller)")
for k, v in callee_by.most_common(top):
    print(f"{v:7d} {100.0 * v / max(total, 1):5.1f}%  {k}")
print("== native bodies called from JS (first frame past the native entry)")
for k, v in native_by.most_common(top):
    print(f"{v:7d} {100.0 * v / max(total, 1):5.1f}%  {k}")
print("== callee leaves")
for k, v in leaf_callee_by.most_common(top):
    print(f"{v:7d} {100.0 * v / max(total, 1):5.1f}%  {k}")
if len(sys.argv) > 6:
    import json
    json.dump({"classes": class_by, "total": total, "self": self_by, "total_by": total_by, "incl": incl_by,
               "residency": residency}, open(sys.argv[6], "w"))
