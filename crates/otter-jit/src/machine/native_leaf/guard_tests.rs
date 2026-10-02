//! Native identity-guard execution against the shared callable header.
//!
//! # Contents
//! Executes the production ARM64 or x86 guard with each native entry kind.
//!
//! # Invariants
//! Synthetic cells retain aligned, initialized storage for the whole leaf call.
//! The executable uses only caller-saved registers and never calls the VM.
//!
//! # See also
//! - [`otter_vm::jit::JitNativeCallLayout`] for the authoritative offsets.

#[cfg(target_arch = "x86_64")]
use dynasmrt::DynasmApi;
use dynasmrt::{AssemblyOffset, DynasmLabelApi, ExecutableBuffer, dynasm};
use otter_vm::{
    JitCompileSnapshot,
    jit::JitNativeCallLayout,
    native_function::{NATIVE_FUNCTION_BODY_TYPE_TAG, NativeEntryKind},
};

fn guard_program(view: &JitCompileSnapshot, identity: u32) -> ExecutableBuffer {
    #[cfg(target_arch = "aarch64")]
    {
        let mut ops = dynasmrt::aarch64::Assembler::new().unwrap();
        let miss = ops.new_dynamic_label();
        crate::template::arm64::ic_probe::emit_native_leaf_guard(&mut ops, view, identity, 0, miss)
            .unwrap();
        dynasm!(ops
            ; .arch aarch64
            ; mov w0, #1
            ; ret
            ; =>miss
            ; mov w0, #0
            ; ret
        );
        ops.finalize().unwrap()
    }
    #[cfg(target_arch = "x86_64")]
    {
        let mut ops = dynasmrt::x64::Assembler::new().unwrap();
        let miss = ops.new_dynamic_label();
        if cfg!(target_os = "windows") {
            dynasm!(ops ; .arch x64 ; mov r10, rcx);
        } else {
            dynasm!(ops ; .arch x64 ; mov r10, rdi);
        }
        super::x86_64::emit_guard(&mut ops, view, identity, miss);
        dynasm!(ops
            ; .arch x64
            ; mov eax, 1
            ; ret
            ; =>miss
            ; xor eax, eax
            ; ret
        );
        ops.finalize().unwrap()
    }
}

#[test]
fn generated_guard_requires_exact_identity_from_c_header() {
    let mut view = JitCompileSnapshot::without_feedback(0, 0, 0, Vec::new());
    view.collection_layout.native_function_type_tag = NATIVE_FUNCTION_BODY_TYPE_TAG;
    view.native_call_layout = JitNativeCallLayout::current();
    let layout = view.native_call_layout;
    let program = guard_program(&view, 0xdead_beef);
    // SAFETY: this exact program uses the platform C ABI, returns a u64,
    // reads only the initialized cell prefix and outlives all calls below.
    let call: extern "C" fn(u64) -> u64 =
        unsafe { std::mem::transmute(program.ptr(AssemblyOffset(0))) };
    let mut cell = [0_u64; 8];
    let bytes = unsafe {
        std::slice::from_raw_parts_mut(cell.as_mut_ptr().cast::<u8>(), std::mem::size_of_val(&cell))
    };
    bytes[0] = NATIVE_FUNCTION_BODY_TYPE_TAG;
    let identity = layout.identity_byte as usize;
    bytes[identity..identity + 4].copy_from_slice(&0xdead_beef_u32.to_ne_bytes());
    for kind in [
        NativeEntryKind::Static,
        NativeEntryKind::StaticWithCaptures,
        NativeEntryKind::VmIntrinsic,
        NativeEntryKind::Dynamic,
        NativeEntryKind::LocalDynamic,
    ] {
        bytes[layout.kind_byte as usize] = kind as u8;
        // The external-ref index names exactly one entry, so the identity
        // alone decides the guard whatever payload kind the header carries.
        assert_eq!(call(bytes.as_ptr() as u64), 1);
    }
    bytes[layout.kind_byte as usize] = NativeEntryKind::Static as u8;
    bytes[identity..identity + 4].copy_from_slice(&0xdead_beee_u32.to_ne_bytes());
    assert_eq!(call(bytes.as_ptr() as u64), 0);
    bytes[identity..identity + 4].copy_from_slice(&0xdead_beef_u32.to_ne_bytes());
    bytes[0] = 0;
    assert_eq!(call(bytes.as_ptr() as u64), 0);
    assert_eq!(call(otter_vm::Value::number_i32(1).to_bits()), 0);
    assert_eq!(call(0), 0);
}
