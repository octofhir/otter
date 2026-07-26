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

use otter_vm::runtime_stubs::leaf_no_alloc_stub2_by_id;
use otter_vm::{JitCompileSnapshot, JitStaticNativeCall};

use crate::{
    artifact::{
        CodeMapCapture, CodeRegion,
        relocation::{RelocationCapture, RelocationTarget},
    },
    entry::{
        NUMBER_TAG_HI16, THREAD_OFFSET, Unsupported, VALUE_UNDEFINED, VM_THREAD_GC_HEAP_OFFSET,
    },
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
    // Support follows the declaration: a target is lowerable exactly when it
    // names a leaf entry. No builtin is named here.
    view.native_static_fn_byte != 0
        && site.argc >= 1
        && leaf_no_alloc_stub2_by_id(site.target.leaf_stub_id).is_some()
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
    second_argument_x: Option<u8>,
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
    // The operation itself lives in the declared leaf entry, so a new builtin
    // costs a descriptor and a Rust body rather than a machine-code arm here.
    // `(heap, arg0, arg1) -> pair`, with no safepoint: a leaf entry cannot
    // allocate, collect, or re-enter JS.
    let Some(stub) = leaf_no_alloc_stub2_by_id(site.target.leaf_stub_id) else {
        return Err(Unsupported::OperandShape("static-native leaf entry"));
    };
    dynasm!(ops
        ; .arch aarch64
        ; mov x10, X(argument_x)
        ; ldr x0, [x20, THREAD_OFFSET]
        ; ldr x0, [x0, VM_THREAD_GC_HEAP_OFFSET]
        ; mov x1, x10
    );
    if let Some(second) = second_argument_x {
        dynasm!(ops ; .arch aarch64 ; mov x2, X(second));
    } else {
        emit_load_u64(ops, 2, VALUE_UNDEFINED);
    }
    emit_load_symbol_u64(
        ops,
        relocations,
        16,
        stub.entry_addr() as u64,
        RelocationTarget::runtime_stub(stub.descriptor),
    );
    dynasm!(ops
        ; .arch aarch64
        ; blr x16
        ; and x1, x1, #0xff
        ; cbnz x1, =>bail
        ; mov x9, x0
    );
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
