"""Count instructions one call spends inside call_trampoline (lldb script).

Usage: lldb --batch -o 'command script import scripts/dev/lldb_count_trampoline.py'
       -o 'process launch --stop-at-entry' -o 'count_trampoline 20000' -- target/release/otter run <script>
Skips N trampoline entries, then single-steps one call and prints the
instructions executed inside the trampoline before the callee entry and after
the callee returns, plus the full trampoline-resident instruction trace.
"""
import lldb


def count_trampoline(debugger, command, result, internal_dict):
    skip = int(command.strip() or "20000")
    target = debugger.GetSelectedTarget()
    module = target.GetModuleAtIndex(0)
    name = "_RNvNtNtCskwZT6iiDpKx_8otter_vm10native_abi15call_trampoline15call_trampoline"
    symbols = module.FindSymbols(name)
    sym = symbols[0].GetSymbol()
    process = target.GetProcess()
    lo = sym.GetStartAddress().GetLoadAddress(target)
    hi = sym.GetEndAddress().GetLoadAddress(target)
    print("trampoline", hex(lo), hex(hi))
    bp = target.BreakpointCreateByAddress(lo)
    bp.SetIgnoreCount(skip)
    process.Continue()
    thread = None
    for candidate in process:
        if candidate.GetStopReason() == lldb.eStopReasonBreakpoint:
            thread = candidate
    process.SetSelectedThread(thread)
    target.BreakpointDelete(bp.GetID())
    def step_inside():
        trace = []
        while True:
            pc = thread.GetFrameAtIndex(0).GetPC()
            if not lo <= pc < hi:
                return trace, pc
            trace.append(pc - lo)
            thread.StepInstruction(False)

    entry_trace, callee_pc = step_inside()
    return_pc = lo + entry_trace[-1] + 4
    frame_sp = thread.GetFrameAtIndex(0).GetSP()
    back = target.BreakpointCreateByAddress(return_pc)
    while True:
        process.Continue()
        if thread.GetFrameAtIndex(0).GetPC() == return_pc:
            break
    target.BreakpointDelete(back.GetID())
    exit_trace, _ = step_inside()
    print("entry instructions", len(entry_trace), "exit instructions", len(exit_trace))
    print("entry", " ".join(hex(o) for o in entry_trace))
    print("exit", " ".join(hex(o) for o in exit_trace))
    process.Kill()


def __lldb_init_module(debugger, internal_dict):
    debugger.HandleCommand("command script add -f lldb_count_trampoline.count_trampoline count_trampoline")
