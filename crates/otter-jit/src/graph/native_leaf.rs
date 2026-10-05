//! Admission of pure declared native calls with an evaluated explicit receiver.
//!
//! # Contents
//! - [`admit`] joins existing feedback and callable declarations to one leaf.
//! - [`entry`] resolves the VM-owned two-value entry for target encoders.
//!
//! # Invariants
//! Exact JavaScript arity and the existing native identity guard precede the
//! call. The admitted entry has no effects, allocation, collection or reentry.
//! Its two boxed operands follow the sole heap/value/value C ABI and result
//! Probe domain. A miss precedes effects and resumes the current CallWithThis;
//! this module never owns a second builtin table or runtime semantic kernel.
//!
//! # See also
//! - `otter_vm::jit_static_native` owns callable identity and operand selection.
//! - `super::ir::Kind::NativeLeaf` owns canonical homes and eager recovery.

use otter_vm::{JitCompileSnapshot, JitStaticNativeCall, native_abi as abi};

/// Exact passive entry behind an existing VM descriptor.
pub(super) fn entry(
    id: abi::RuntimeStubId,
) -> Result<otter_vm::runtime_stubs::LeafNoAllocStub2, crate::Unsupported> {
    otter_vm::runtime_stubs::leaf_no_alloc_stub2_by_id(id)
        .filter(|stub| {
            let descriptor = stub.descriptor;
            stub.is_valid()
                && descriptor.class == abi::RuntimeStubClass::LeafNoAlloc
                && descriptor.signature == abi::RuntimeStubSignature::LeafValue2
                && descriptor.safepoint == abi::RuntimeStubSafepoint::Forbidden
                && descriptor.exception == abi::RuntimeStubException::Never
                && descriptor.effects.bits() == 0
                && descriptor.result_abi == abi::RuntimeStubResultAbi::NativePair
                && descriptor.result_domain == abi::NativeResultDomain::Probe
        })
        .ok_or(crate::Unsupported::OperandShape(
            "Graph pure native leaf entry",
        ))
}

/// Admit one actual explicit-receiver call, without adapting arity or effects.
pub(super) fn admit(
    view: &JitCompileSnapshot,
    target: JitStaticNativeCall,
    argument_count: usize,
) -> Option<&'static otter_vm::jit_static_native::JitLeafBuiltin> {
    let declaration = otter_vm::jit_static_native::jit_leaf_builtin(target.leaf_stub_id)?;
    entry(target.leaf_stub_id).ok()?;
    (view.native_call_layout.identity_byte != 0
        && argument_count == usize::from(declaration.argument_count)
        && target.argument_count == declaration.argument_count
        && declaration.operand_words() <= 2)
        .then_some(declaration)
}

#[cfg(test)]
mod tests;
