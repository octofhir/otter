#!/usr/bin/env python3
"""Exact retired-instruction path of one steady-state stretch, by function.

usage: step_count.py <steps> <break-symbol> <skip-hits> -- <command...>
Runs the command under lldb, stops at the <skip-hits>-th hit of
<break-symbol> (a substring-free exact symbol regex is fine), then
single-steps <steps> machine instructions, counting each by the function
that holds it. Prints the per-function counts, then the call sequence
(consecutive runs collapsed).
"""
import collections, subprocess, sys, tempfile, os

steps, sym, skip = int(sys.argv[1]), sys.argv[2], int(sys.argv[3])
cmd = sys.argv[sys.argv.index("--") + 1:]
script = f'''
import lldb, collections
def run(debugger, command, result, internal_dict):
    target = debugger.GetSelectedTarget()
    process = target.GetProcess()
    thread = process.GetSelectedThread()
    counts = collections.Counter()
    seq = []
    for _ in range({steps}):
        frame = thread.GetFrameAtIndex(0)
        name = frame.GetFunctionName() or frame.GetSymbol().GetName() or hex(frame.GetPC())
        counts[name] += 1
        if not seq or seq[-1][0] != name:
            seq.append([name, 0])
        seq[-1][1] += 1
        thread.StepInstruction(False)
    out = open("{tempfile.gettempdir()}/step_count.out", "w")
    for name, n in counts.most_common():
        out.write(f"{{n:6d}}  {{name}}\\n")
    out.write("== sequence\\n")
    for name, n in seq:
        out.write(f"{{n:6d}}  {{name}}\\n")
    out.close()
'''
d = tempfile.mkdtemp()
py = os.path.join(d, "stepper.py")
open(py, "w").write(script)
lldb_cmds = [
    "settings set target.process.stop-on-exec false",
    f"command script import {py}",
    f"breakpoint set -r '{sym}'",
    f"breakpoint modify -i {skip} 1",
    "process launch",
    "breakpoint disable",
    "command script add -f stepper.run stepcount",
    "stepcount",
    "process kill",
    "quit",
]
args = ["lldb", "--batch"]
for c in lldb_cmds:
    args += ["-o", c]
args += ["--"] + cmd
subprocess.run(args, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
print(open(os.path.join(tempfile.gettempdir(), "step_count.out")).read())
