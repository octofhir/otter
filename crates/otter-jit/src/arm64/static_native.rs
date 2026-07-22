//! Guarded static-native leaves shared by AArch64 JIT tiers.
//!
//! # Contents
//! - Exact native-function type and bootstrap-address guards.
//! - Direct numeric machine code for supported extracted builtins.
//! - Structured code-map and relocation capture for both phases.
//!
//! # Invariants
//! - Every guard miss branches to the caller's exact pre-effect side exit.
//! - Leaves never call Rust, allocate, publish a safepoint, or re-enter JS.
//! - Raw bootstrap addresses are emitted only through typed relocations and
//!   never appear in normalized artifacts.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, aarch64::Assembler, dynasm};

use otter_vm::{JitCompileSnapshot, JitStaticNativeCall, JitStaticNativeCallKind};

use crate::{
    artifact::{
        CodeMapCapture, CodeRegion,
        relocation::{RelocationCapture, RelocationTarget},
    },
    entry::{NUMBER_TAG_HI16, Unsupported},
    template::arm64::values::{emit_box_double, emit_box_int32, emit_num_to_double},
};

/// Static metadata naming one emitted ordinary-call leaf.
#[derive(Clone, Copy)]
pub(crate) struct StaticNativeCallSite<'a> {
    pub(crate) target: &'a JitStaticNativeCall,
    pub(crate) caller_function_id: u32,
    pub(crate) logical_pc: u32,
    pub(crate) byte_pc: u32,
    pub(crate) argc: usize,
}

/// Whether the current layout can emit this static-native operation.
pub(crate) fn target_is_supported(
    view: &JitCompileSnapshot,
    site: StaticNativeCallSite<'_>,
) -> bool {
    view.native_static_fn_byte != 0
        && site.argc >= 1
        && matches!(site.target.kind, JitStaticNativeCallKind::MathAbs)
}

/// Emit an identity-guarded leaf.
///
/// `callee_x` and `argument_x` contain tagged values. The boxed result is left
/// in `x9`; scratch registers are `x11..x15` and `d0`.
pub(crate) fn emit_static_native_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    view: &JitCompileSnapshot,
    site: StaticNativeCallSite<'_>,
    callee_x: u8,
    argument_x: u8,
    mut code_map: Option<&mut CodeMapCapture>,
    bail: DynamicLabel,
) -> Result<(), Unsupported> {
    if !target_is_supported(view, site) {
        return Err(Unsupported::OperandShape(
            "static-native call target layout",
        ));
    }

    let guard_start = ops.offset().0;
    dynasm!(ops
        ; .arch aarch64
        ; movz x11, NUMBER_TAG_HI16, lsl #48
        ; orr x11, x11, #0x2       // NOT_CELL_MASK
        ; tst X(callee_x), x11
        ; b.ne =>bail
        ; cbz X(callee_x), =>bail
    );
    let native_type_tag = u32::from(view.collection_layout.native_function_type_tag);
    dynasm!(ops
        ; .arch aarch64
        ; ldrb w14, [X(callee_x)]
        ; cmp w14, native_type_tag
        ; b.ne =>bail
        ; ldr x14, [X(callee_x), view.native_static_fn_byte]
    );
    emit_load_symbol_u64(
        ops,
        relocations,
        15,
        site.target.builtin_fn_addr as u64,
        RelocationTarget::StaticNativeBuiltinFunction {
            target: site.target.kind,
            byte_pc: site.byte_pc,
        },
    );
    dynasm!(ops
        ; .arch aarch64
        ; cmp x14, x15
        ; b.ne =>bail
    );
    let guard_end = ops.offset().0;
    if let Some(code_map) = code_map.as_deref_mut() {
        code_map.record(CodeRegion::static_native_structural(
            "staticNativeCallGuard",
            guard_start,
            guard_end,
            site.caller_function_id,
            site.logical_pc,
            site.byte_pc,
            site.target.kind,
        ));
    }

    let body_start = ops.offset().0;
    match site.target.kind {
        JitStaticNativeCallKind::MathAbs => {
            let double_path = ops.new_dynamic_label();
            let nonnegative = ops.new_dynamic_label();
            let done = ops.new_dynamic_label();
            dynasm!(ops
                ; .arch aarch64
                ; movz x15, NUMBER_TAG_HI16, lsl #48
                ; and x14, X(argument_x), x15
                ; cmp x14, x15
                ; b.ne =>double_path
                ; cmp W(argument_x), wzr
                ; b.ge =>nonnegative
                ; negs w9, W(argument_x)
                ; b.vs =>double_path
            );
            emit_box_int32(ops, 9, 14);
            dynasm!(ops
                ; .arch aarch64
                ; b =>done
                ; =>nonnegative
                ; mov x9, X(argument_x)
                ; b =>done
                ; =>double_path
            );
            emit_num_to_double(ops, argument_x, 0, bail);
            dynasm!(ops ; .arch aarch64 ; fabs d0, d0);
            emit_box_double(ops, 0, 9);
            dynasm!(ops ; .arch aarch64 ; =>done);
        }
    }
    if let Some(code_map) = code_map {
        code_map.record(CodeRegion::static_native_structural(
            "staticNativeCallBody",
            body_start,
            ops.offset().0,
            site.caller_function_id,
            site.logical_pc,
            site.byte_pc,
            site.target.kind,
        ));
    }
    Ok(())
}

fn emit_load_u64(ops: &mut Assembler, register: u8, value: u64) {
    dynasm!(ops ; .arch aarch64 ; movz X(register), (value & 0xffff) as u32);
    if (value >> 16) & 0xffff != 0 {
        dynasm!(ops
            ; .arch aarch64
            ; movk X(register), ((value >> 16) & 0xffff) as u32, lsl #16
        );
    }
    if (value >> 32) & 0xffff != 0 {
        dynasm!(ops
            ; .arch aarch64
            ; movk X(register), ((value >> 32) & 0xffff) as u32, lsl #32
        );
    }
    if (value >> 48) & 0xffff != 0 {
        dynasm!(ops
            ; .arch aarch64
            ; movk X(register), ((value >> 48) & 0xffff) as u32, lsl #48
        );
    }
}

fn emit_load_symbol_u64(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    register: u8,
    value: u64,
    target: RelocationTarget,
) {
    let start = ops.offset().0;
    emit_load_u64(ops, register, value);
    relocations.record_mov_wide(start, ops.offset().0, register, target);
}
