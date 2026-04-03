//! Small host helpers for the tier1 runtime path.

use crate::context::JitContext;
use crate::{BAILOUT_SENTINEL, BailoutReason};
use otter_vm::object::{ObjectHandle, PropertyValue};
use otter_vm::{FunctionIndex, Module, ObjectShapeId, RegisterValue, RuntimeState};

const MAX_DIRECT_CALL_ARGS: usize = 8;

fn write_bailout(ctx: &mut JitContext, reason: BailoutReason, bytecode_pc: u32) -> u64 {
    ctx.bailout_reason = reason as u32;
    ctx.bailout_pc = bytecode_pc;
    BAILOUT_SENTINEL
}

unsafe fn module(ctx: &JitContext) -> Option<&Module> {
    let ptr = ctx.module_ptr.cast::<Module>();
    (!ptr.is_null()).then(|| unsafe { &*ptr })
}

unsafe fn runtime(ctx: &mut JitContext) -> Option<&mut RuntimeState> {
    let ptr = ctx.runtime_ptr.cast::<RuntimeState>();
    (!ptr.is_null()).then(|| unsafe { &mut *ptr })
}

pub extern "C" fn otter_get_prop_shaped(
    ctx: *mut JitContext,
    obj_handle: i64,
    shape_id: i64,
    slot_index: i64,
    bytecode_pc: i64,
) -> i64 {
    let Some(ctx) = (unsafe { ctx.as_mut() }) else {
        return BAILOUT_SENTINEL as i64;
    };
    let Some(runtime) = (unsafe { runtime(ctx) }) else {
        return write_bailout(ctx, BailoutReason::Unsupported, bytecode_pc as u32) as i64;
    };
    let Some(handle) = u32::try_from(obj_handle).ok().map(ObjectHandle) else {
        return write_bailout(ctx, BailoutReason::ShapeGuardFailed, bytecode_pc as u32) as i64;
    };
    let Some(shape_id) = u64::try_from(shape_id).ok().map(ObjectShapeId) else {
        return write_bailout(ctx, BailoutReason::ShapeGuardFailed, bytecode_pc as u32) as i64;
    };
    let Some(slot_index) = u16::try_from(slot_index).ok() else {
        return write_bailout(ctx, BailoutReason::ShapeGuardFailed, bytecode_pc as u32) as i64;
    };
    match runtime.objects().get_shaped(handle, shape_id, slot_index) {
        Ok(Some(PropertyValue::Data { value, .. })) => value.raw_bits() as i64,
        Ok(Some(PropertyValue::Accessor { .. })) => {
            write_bailout(ctx, BailoutReason::Unsupported, bytecode_pc as u32) as i64
        }
        Ok(None) => write_bailout(ctx, BailoutReason::ShapeGuardFailed, bytecode_pc as u32) as i64,
        Err(_) => write_bailout(ctx, BailoutReason::Unsupported, bytecode_pc as u32) as i64,
    }
}

pub extern "C" fn otter_set_prop_shaped(
    ctx: *mut JitContext,
    obj_handle: i64,
    shape_id: i64,
    slot_index: i64,
    value_bits: i64,
    bytecode_pc: i64,
) -> i64 {
    let Some(ctx) = (unsafe { ctx.as_mut() }) else {
        return BAILOUT_SENTINEL as i64;
    };
    let Some(runtime) = (unsafe { runtime(ctx) }) else {
        return write_bailout(ctx, BailoutReason::Unsupported, bytecode_pc as u32) as i64;
    };
    let Some(handle) = u32::try_from(obj_handle).ok().map(ObjectHandle) else {
        return write_bailout(ctx, BailoutReason::ShapeGuardFailed, bytecode_pc as u32) as i64;
    };
    let Some(shape_id) = u64::try_from(shape_id).ok().map(ObjectShapeId) else {
        return write_bailout(ctx, BailoutReason::ShapeGuardFailed, bytecode_pc as u32) as i64;
    };
    let Some(slot_index) = u16::try_from(slot_index).ok() else {
        return write_bailout(ctx, BailoutReason::ShapeGuardFailed, bytecode_pc as u32) as i64;
    };
    let Some(value) = RegisterValue::from_raw_bits(value_bits as u64) else {
        return write_bailout(ctx, BailoutReason::Unsupported, bytecode_pc as u32) as i64;
    };
    match runtime
        .objects_mut()
        .set_shaped(handle, shape_id, slot_index, value)
    {
        Ok(true) => 0,
        Ok(false) => write_bailout(ctx, BailoutReason::ShapeGuardFailed, bytecode_pc as u32) as i64,
        Err(_) => write_bailout(ctx, BailoutReason::Unsupported, bytecode_pc as u32) as i64,
    }
}

#[allow(clippy::too_many_arguments)]
pub extern "C" fn otter_call_direct(
    ctx: *mut JitContext,
    callee_index: i64,
    bytecode_pc: i64,
    argc: i64,
    arg0: i64,
    arg1: i64,
    arg2: i64,
    arg3: i64,
    arg4: i64,
    arg5: i64,
    arg6: i64,
    arg7: i64,
) -> i64 {
    let Some(ctx) = (unsafe { ctx.as_mut() }) else {
        return BAILOUT_SENTINEL as i64;
    };
    let Some(module) = (unsafe { module(ctx) }) else {
        return write_bailout(ctx, BailoutReason::Unsupported, bytecode_pc as u32) as i64;
    };
    let Some(callee_index) = u32::try_from(callee_index).ok().map(FunctionIndex) else {
        return write_bailout(ctx, BailoutReason::CallTargetMismatch, bytecode_pc as u32) as i64;
    };
    let Some(argc) = usize::try_from(argc).ok() else {
        return write_bailout(ctx, BailoutReason::Unsupported, bytecode_pc as u32) as i64;
    };
    if argc > MAX_DIRECT_CALL_ARGS {
        return write_bailout(ctx, BailoutReason::Unsupported, bytecode_pc as u32) as i64;
    }
    let Some(function) = module.function(callee_index) else {
        return write_bailout(ctx, BailoutReason::CallTargetMismatch, bytecode_pc as u32) as i64;
    };

    let raw_args = [arg0, arg1, arg2, arg3, arg4, arg5, arg6, arg7];
    let mut callee_registers =
        vec![RegisterValue::undefined(); usize::from(function.frame_layout().register_count())];
    let parameter_range = function.frame_layout().parameter_range();
    for (offset, raw) in raw_args.into_iter().take(argc).enumerate() {
        let dst = parameter_range
            .start()
            .saturating_add(u16::try_from(offset).unwrap_or(u16::MAX));
        if dst >= parameter_range.end() {
            break;
        }
        let Some(value) = RegisterValue::from_raw_bits(raw as u64) else {
            return write_bailout(ctx, BailoutReason::Unsupported, bytecode_pc as u32) as i64;
        };
        callee_registers[usize::from(dst)] = value;
    }

    match crate::deopt::execute_function_with_fallback(
        module,
        callee_index,
        &mut callee_registers,
        ctx.interrupt_flag,
    ) {
        Ok(result) => result.return_value().raw_bits() as i64,
        Err(_) => write_bailout(ctx, BailoutReason::Unsupported, bytecode_pc as u32) as i64,
    }
}
