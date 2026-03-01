use std::sync::OnceLock;

use otter_vm_bytecode::Function;
use otter_vm_jit::runtime_helpers::RuntimeHelpers;

use crate::jit_helpers::{self, JitContext};
use crate::value::Value;

static RUNTIME_HELPERS: OnceLock<RuntimeHelpers> = OnceLock::new();

pub(crate) use otter_vm_exec::JitExecResult;

pub(crate) fn runtime_helpers() -> &'static RuntimeHelpers {
    RUNTIME_HELPERS.get_or_init(jit_helpers::build_runtime_helpers)
}

/// Try to execute JIT-compiled code for a function.
///
/// Builds the per-call `JitContext` (VM pointers + snapshots) and delegates
/// machine-code dispatch/deopt accounting to `otter-vm-exec`.
pub(crate) fn try_execute_jit(
    module_id: u64,
    function_index: u32,
    function: &Function,
    args: &[Value],
    proto_epoch: u64,
    interpreter: *const crate::interpreter::Interpreter,
    vm_ctx: *mut crate::context::VmContext,
    constants: *const otter_vm_bytecode::ConstantPool,
    upvalues: &[crate::value::UpvalueCell],
) -> JitExecResult {
    let this_raw = if vm_ctx.is_null() {
        Value::undefined().to_jit_bits()
    } else {
        let vm = unsafe { &*vm_ctx };
        let pending = vm.pending_this_to_trace().cloned();
        let this_val = pending.unwrap_or_else(Value::undefined);
        if !function.flags.is_strict && (this_val.is_undefined() || this_val.is_null()) {
            Value::object(vm.global()).to_jit_bits()
        } else {
            this_val.to_jit_bits()
        }
    };

    let jit_ctx = JitContext {
        function_ptr: function as *const Function,
        proto_epoch,
        interpreter,
        vm_ctx,
        constants,
        upvalues_ptr: if upvalues.is_empty() {
            std::ptr::null()
        } else {
            upvalues.as_ptr()
        },
        upvalue_count: upvalues.len() as u32,
        this_raw,
        callee_raw: if vm_ctx.is_null() {
            Value::undefined().to_jit_bits()
        } else {
            let vm = unsafe { &*vm_ctx };
            vm.pending_callee_to_trace()
                .cloned()
                .unwrap_or_else(Value::undefined)
                .to_jit_bits()
        },
        home_object_raw: if vm_ctx.is_null() {
            Value::null().to_jit_bits()
        } else {
            let vm = unsafe { &*vm_ctx };
            vm.pending_home_object_to_trace()
                .map(|obj| Value::object(obj.clone()).to_jit_bits())
                .unwrap_or_else(|| Value::null().to_jit_bits())
        },
        secondary_result: 0,
    };

    let ctx_ptr = &jit_ctx as *const JitContext as *mut u8;
    let argc = args.len() as u32;

    if args.len() <= 8 {
        let mut inline = [0_i64; 8];
        for (idx, arg) in args.iter().enumerate() {
            inline[idx] = arg.to_jit_bits();
        }
        return otter_vm_exec::try_execute_jit_raw(
            module_id,
            function_index,
            function,
            argc,
            inline.as_ptr(),
            ctx_ptr,
        );
    }

    let mut arg_bits = Vec::with_capacity(args.len());
    for arg in args {
        arg_bits.push(arg.to_jit_bits());
    }

    otter_vm_exec::try_execute_jit_raw(
        module_id,
        function_index,
        function,
        argc,
        arg_bits.as_ptr(),
        ctx_ptr,
    )
}
