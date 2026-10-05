//! Static native leaf calls of the x86-64 template tier.
//!
//! # Contents
//! - [`supports_site`] — whether a static native call site is a leaf the
//!   template enters without a frame.
//! - [`emit_guard`] proves the exact bootstrap native-function identity.
//! - [`emit_tagged_call`] enters the shared non-allocating leaf ABI (pure and
//!   in-place mutating families).
//!
//! # Invariants
//! - The identity guard completes before a native call has an observable
//!   effect.
//! - Static kind and external identity come from the VM's one C callable header.
//! - Tagged leaves receive `(heap, value0, value1[, value2])` through the
//!   platform C ABI and normalize the shared pair into `rax`/`rdx`.
//! - No path allocates, collects, throws, or publishes a safepoint.
//!
//! # See also
//! - [`otter_vm::jit::JitNativeCallLayout`] for shared callable offsets.
//! - `otter_vm::jit_static_native` for leaf declarations.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_vm::{JitCompileSnapshot, JitStaticNativeCall, Value, native_abi::RuntimeStubId};

use crate::{
    Unsupported,
    artifact::relocation::{RelocationCapture, RelocationTarget},
    entry::{THREAD_OFFSET, VM_THREAD_GC_HEAP_OFFSET},
};

const NOT_CELL_MASK: u64 = otter_vm::value::tag::NOT_CELL_MASK;

/// Whether the static native call at a site with `argument_count` actuals
/// enters `target` as a leaf: a plain call of a declared entry that reads no
/// `this`, with exactly its declared operand words.
pub(crate) fn supports_site(
    view: &otter_vm::JitCompileSnapshot,
    target: JitStaticNativeCall,
    argument_count: usize,
) -> bool {
    let Some(declaration) = otter_vm::jit_static_native::jit_leaf_builtin(target.leaf_stub_id)
    else {
        return false;
    };
    // A plain call passes no receiver word, so an entry reading `this` is
    // lowered only by the explicit-receiver probe.
    view.native_call_layout.identity_byte != 0
        && !declaration.this_operand
        && argument_count == usize::from(declaration.argument_count)
        && target.argument_count == declaration.argument_count
        && otter_vm::runtime_stubs::leaf_no_alloc_stub2_by_id(target.leaf_stub_id)
            .is_some_and(|stub| stub.is_valid())
}

/// Prove that `r10` still names the exact bootstrap native callable.
pub(crate) fn emit_guard(
    ops: &mut Assembler,
    view: &JitCompileSnapshot,
    builtin_native_ref: u32,
    miss: DynamicLabel,
) {
    load_u64(ops, 11, NOT_CELL_MASK);
    dynasm!(ops
        ; .arch x64
        ; mov rax, r10
        ; and rax, r11
        ; jnz =>miss
        ; test r10, r10
        ; jz =>miss
        ; cmp BYTE [r10], view.collection_layout.native_function_type_tag as i8
        ; jne =>miss
        ; cmp DWORD [r10 + view.native_call_layout.identity_byte as i32], builtin_native_ref as i32
        ; jne =>miss
    );
}

/// Call a declared non-allocating leaf entry once its guards passed.
///
/// `words` operand words are already in `rsi`/`rdx`/`rcx`; an unfilled word
/// of the entry's family is `undefined`. Pure and in-place mutating families
/// share `(heap, value0, value1[, value2]) -> pair`. The boxed result is left
/// in `rax`; a miss branches to `miss`.
pub(crate) fn emit_tagged_call(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    stub: RuntimeStubId,
    words: u8,
    miss: DynamicLabel,
) -> Result<(), Unsupported> {
    use otter_vm::runtime_stubs::{
        leaf_no_alloc_stub2_by_id, mutating_leaf_stub2_by_id, mutating_leaf_stub3_by_id,
    };
    let Some((entry, descriptor, family_words)) = leaf_no_alloc_stub2_by_id(stub)
        .filter(|stub| stub.is_valid())
        .map(|stub| (stub.entry_addr(), stub.descriptor, 2))
        .or_else(|| {
            mutating_leaf_stub2_by_id(stub)
                .filter(|stub| stub.is_valid())
                .map(|stub| (stub.entry_addr(), stub.descriptor, 2))
        })
        .or_else(|| {
            mutating_leaf_stub3_by_id(stub)
                .filter(|stub| stub.is_valid())
                .map(|stub| (stub.entry_addr(), stub.descriptor, 3))
        })
        .filter(|&(_, _, family_words)| words <= family_words)
    else {
        return Err(Unsupported::OperandShape("x86-64 native leaf entry"));
    };
    // Prepared word arguments after the heap pointer: rsi, rdx, rcx.
    // The shared C boundary owns platform argument and result placement.
    for register in [6_u8, 2, 1]
        .into_iter()
        .take(usize::from(family_words))
        .skip(usize::from(words))
    {
        load_u64(ops, register, Value::undefined().to_bits());
    }
    dynasm!(ops
        ; .arch x64
        ; mov rdi, [r15 + THREAD_OFFSET as i32]
        ; mov rdi, [rdi + VM_THREAD_GC_HEAP_OFFSET as i32]
    );
    let start = ops.offset().0;
    load_u64(ops, 11, entry as u64);
    relocations.record_x86_imm64(
        start,
        ops.offset().0,
        11,
        RelocationTarget::runtime_stub(descriptor),
    );
    crate::x86_64::call_abi::emit_runtime_call(ops, descriptor);
    dynasm!(ops
        ; .arch x64
        ; test rdx, rdx
        ; jne =>miss
    );
    Ok(())
}

#[allow(clippy::useless_conversion)]
fn load_u64(ops: &mut Assembler, register: u8, value: u64) {
    dynasm!(ops ; .arch x64 ; mov Rq(register), QWORD value as i64);
}
