//! JIT runtime — bridges the interpreter to the new otter-jit pipeline.

use crate::value::Value;
use otter_vm_bytecode::Function;

/// Deopt value slot — preserved for context.rs compatibility.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct DeoptValueSlot {
    pub index: u16,
    pub value: Value,
}

/// Result of attempting JIT execution at the otter-vm-core level.
pub(crate) enum JitCallResult {
    Ok(Value),
    Bailout { bytecode_pc: u32 },
    NotCompiled,
}

/// Attempt to execute a function via the new JIT pipeline.
#[allow(clippy::too_many_arguments)]
pub(crate) fn try_execute_jit(
    function: &Function,
    registers_ptr: *mut Value,
    register_base: usize,
    this_value: Value,
    constants: *const otter_vm_bytecode::ConstantPool,
    interpreter: *const crate::interpreter::Interpreter,
    vm_ctx: *mut crate::context::VmContext,
    upvalues: &[crate::value::UpvalueCell],
    callee_raw: u64,
    home_object_raw: u64,
    proto_epoch: u64,
    interrupt_flag: *const u8,
) -> JitCallResult {
    let registers_base = unsafe { registers_ptr.add(register_base) as *mut u64 };

    let result = unsafe {
        otter_jit::pipeline::try_execute(
            function,
            registers_base,
            function.local_count as u32,
            function.register_count as u32,
            this_value.to_jit_bits() as u64,
            constants as *const (),
            interpreter as *const (),
            vm_ctx as *mut (),
            if upvalues.is_empty() {
                std::ptr::null()
            } else {
                upvalues.as_ptr() as *const ()
            },
            upvalues.len() as u32,
            callee_raw,
            home_object_raw,
            proto_epoch,
            interrupt_flag,
        )
    };

    match result {
        otter_jit::pipeline::JitExecResult::Ok(bits) => {
            if let Some(value) = Value::from_jit_bits(bits) {
                JitCallResult::Ok(value)
            } else {
                JitCallResult::NotCompiled
            }
        }
        otter_jit::pipeline::JitExecResult::Bailout { bytecode_pc, .. } => {
            JitCallResult::Bailout { bytecode_pc }
        }
        otter_jit::pipeline::JitExecResult::NotCompiled => JitCallResult::NotCompiled,
    }
}

/// Public JIT stats — uses new otter-jit telemetry.
#[derive(Debug, Clone, Default)]
pub struct JitRuntimeStats {
    pub compiled_functions: u64,
    pub total_compile_time_ns: u64,
    pub total_code_bytes: u64,
}

#[derive(Debug, Clone)]
pub struct JitHelperCallStat {
    pub name: String,
    pub calls: u64,
}

/// Initialize JIT helpers. Call once during VM startup.
pub fn init_jit_helpers() {
    let symbols = crate::jit_helpers::collect_helper_symbols();
    otter_jit::pipeline::register_helper_symbols(symbols);
}

/// Returns JIT stats from the new pipeline.
pub fn stats_snapshot() -> JitRuntimeStats {
    let snap = otter_jit::telemetry::snapshot();
    JitRuntimeStats {
        compiled_functions: snap.tier1_compile_times_ns.len() as u64
            + snap.tier2_compile_times_ns.len() as u64,
        total_compile_time_ns: snap.tier1_compile_times_ns.iter().sum::<u64>()
            + snap.tier2_compile_times_ns.iter().sum::<u64>(),
        total_code_bytes: 0, // TODO: track code size in telemetry
    }
}
