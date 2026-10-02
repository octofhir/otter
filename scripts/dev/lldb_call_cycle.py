"""Count instructions between consecutive entries of a hot generated callee.

Usage: lldb --batch -p <pid> -o 'command script import scripts/dev/lldb_call_cycle.py'
       -o 'call_cycle 8'
Attaches to a hot process, single-steps until a branch-with-link from
generated code lands in generated code, and takes that landing PC (a call
entry) as the cycle anchor. Then it single-steps the given number of complete
cycles between consecutive arrivals at the anchor, attributing every executed
instruction to the call trampoline, other Rust code or generated code
(unknown module), with per-cycle totals and the generated-code PCs in
execution order.
"""
import time

import lldb


def _generated(frame):
    return not frame.GetModule().IsValid()


def _mnemonic(target, pc):
    address = lldb.SBAddress(pc, target)
    instructions = target.ReadInstructions(address, 1)
    if instructions.GetSize() == 0:
        return ""
    return instructions.GetInstructionAtIndex(0).GetMnemonic(target)


def _find_anchor(target, thread, limit=2_000_000):
    previous = None
    for _ in range(limit):
        frame = thread.GetFrameAtIndex(0)
        pc = frame.GetPC()
        if previous is not None and _generated(frame) and pc != previous[0] + 4:
            if previous[1] and _mnemonic(target, previous[0]).startswith("bl"):
                return pc
        previous = (pc, _generated(frame))
        thread.StepInstruction(False)
    raise RuntimeError("no generated-to-generated call within the step limit")


def _generated_thread(debugger, process, attempts=50):
    """A thread stopped in generated code; the JS thread is not the main one."""
    debugger.SetAsync(False)
    for _ in range(attempts):
        for thread in process:
            if _generated(thread.GetFrameAtIndex(0)):
                process.SetSelectedThread(thread)
                return thread
        process.Continue()
        time.sleep(0.01)
        process.Stop()
    raise RuntimeError("no thread stopped in generated code")


def call_cycle(debugger, command, result, internal_dict):
    cycles = int(command.strip() or "8")
    target = debugger.GetSelectedTarget()
    process = target.GetProcess()
    thread = _generated_thread(debugger, process)
    anchor = _find_anchor(target, thread)
    print("anchor", hex(anchor))
    for _ in range(cycles):
        counts = {"trampoline": 0, "rust": 0, "generated": 0}
        rust_names = {}
        trace = []
        first = True
        while True:
            frame = thread.GetFrameAtIndex(0)
            pc = frame.GetPC()
            if pc == anchor and not first:
                break
            first = False
            name = frame.GetSymbol().GetName() or ""
            if "call_trampoline" in name or "call_generic" in name:
                counts["trampoline"] += 1
            elif not _generated(frame):
                counts["rust"] += 1
                rust_names[name] = rust_names.get(name, 0) + 1
            else:
                counts["generated"] += 1
                trace.append(pc)
            thread.StepInstruction(False)
        print(counts, sorted(rust_names.items(), key=lambda kv: -kv[1])[:4])
        print("pcs", " ".join(hex(pc) for pc in trace))
    process.Detach()


def __lldb_init_module(debugger, internal_dict):
    debugger.HandleCommand("command script add -f lldb_call_cycle.call_cycle call_cycle")
