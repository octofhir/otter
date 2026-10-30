#!/usr/bin/env python3
"""Mean share per Web Tooling Benchmark target of each hot function, over the
`sample` reports scripts/dev/web_tooling_profile.sh wrote.

usage: web_tooling_hot.py <report-dir> [self|exits] [top]
`self` (default) ranks leaf functions by self samples (isolate_self.py);
`exits` ranks the runtime entries generated code called (jit_exits.py). Each
row shows the mean share over all targets and the targets where it is largest.
"""
import collections, os, re, subprocess, sys

root = sys.argv[1]
mode = sys.argv[2] if len(sys.argv) > 2 else "self"
top = int(sys.argv[3]) if len(sys.argv) > 3 else 40
tool = {"self": "isolate_self.py", "exits": "jit_exits.py"}[mode]
here = os.path.dirname(os.path.abspath(__file__))
shares = collections.defaultdict(dict)
targets = []
for name in sorted(os.listdir(root)):
    if not name.endswith(".txt"):
        continue
    target = name[:-4]
    out = subprocess.run(
        [sys.executable, os.path.join(here, tool), os.path.join(root, name), "400"],
        capture_output=True, text=True,
    ).stdout
    targets.append(target)
    for line in out.splitlines():
        m = re.match(r"^\s*\d+\s+([\d.]+)%\s+(.*)$", line)
        if m:
            shares[m.group(2).strip()][target] = float(m.group(1))
rows = sorted(shares.items(), key=lambda kv: -sum(kv[1].values()))
for fn, per in rows[:top]:
    mean = sum(per.values()) / len(targets)
    worst = sorted(per.items(), key=lambda kv: -kv[1])[:3]
    tops = " ".join(f"{t}:{v:.1f}" for t, v in worst)
    print(f"{mean:5.2f}%  {fn[:90]:90}  {tops}")
