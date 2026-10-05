//! Executable exact-frame DerivedThis identity publication.
//!
//! # Contents
//! - Actual target publisher, large source ids and ordinary/inline refusal.
//! - Complete adjacent compressed root/receiver words remain untouched.
//!
//! # Invariants
//! This engine-private fixture has no GC or JS boundary. It executes the sole
//! helper used by both tiers and preserves host ABI registers. Raw offset
//! access is confined to the VM-owned hidden frame layout, not a public API.
//!
//! # See also
//! - `otter_vm::constructor_layout::tests` owns real moving identity aliases.

use crate::CompiledCode;
use dynasmrt::{DynasmApi, dynasm};
use otter_vm::{
    Value,
    native_abi::{self as abi, Frame, VmFrameHeader},
};

#[test]
fn derived_context_publication_requires_exact_construct_frame_and_preserves_root_words() {
    let fid = 0x12_345;
    let result = 0x1357_0000_1080u64;
    #[cfg(target_arch = "aarch64")]
    let mut ops = dynasmrt::aarch64::Assembler::new().unwrap();
    #[cfg(target_arch = "x86_64")]
    let mut ops = dynasmrt::x64::Assembler::new().unwrap();
    let entry = ops.offset();
    #[cfg(target_arch = "aarch64")]
    {
        crate::arm64::allocation::emit_publish_derived_this_context(&mut ops, 0, 1, fid, [9, 10]);
        dynasm!(ops ; .arch aarch64 ; mov x0, x1 ; ret);
    }
    #[cfg(target_arch = "x86_64")]
    {
        if cfg!(target_os = "windows") {
            dynasm!(ops ; .arch x64 ; mov r8, rcx);
        } else {
            dynasm!(ops ; .arch x64 ; mov r8, rdi ; mov rdx, rsi);
        }
        crate::x86_64::allocation::emit_publish_derived_this_context(&mut ops, 8, 2, fid, [10, 11]);
        dynasm!(ops ; .arch x64 ; mov rax, rdx ; ret);
    }
    let code = CompiledCode::new(ops.finalize().unwrap(), entry);
    let run: unsafe extern "C" fn(*mut Frame, u64) -> u64 =
        unsafe { std::mem::transmute(code.entry_ptr()) };
    for (derived, frame_fid) in [(true, fid), (false, fid), (true, fid + 1)] {
        let mut frame = Frame::new(
            VmFrameHeader::interpreter(frame_fid, 0),
            0,
            Value::function(frame_fid),
            Value::hole(),
        );
        if derived {
            frame.set_derived_constructor();
        }
        let base = std::ptr::from_mut(&mut frame).cast::<u8>();
        // SAFETY: this private fixture accesses only initialized exact scalar
        // words by the authoritative ABI offsets; no collector observes it.
        unsafe {
            base.add(abi::NATIVE_FRAME_CONSTRUCT_LAYOUT_OFFSET as usize)
                .cast::<u32>()
                .write(0x2468);
            base.add(abi::NATIVE_FRAME_DERIVED_THIS_CONTEXT_OFFSET as usize)
                .cast::<u32>()
                .write(0x3579);
            base.add(abi::NATIVE_FRAME_CONSTRUCT_RECEIVER_OFFSET as usize)
                .cast::<u64>()
                .write(0x51a7_0123_4567_89ab);
            assert_eq!(run(&mut frame, result), result);
            assert_eq!(
                base.add(abi::NATIVE_FRAME_CONSTRUCT_LAYOUT_OFFSET as usize)
                    .cast::<u32>()
                    .read(),
                0x2468
            );
            assert_eq!(
                base.add(abi::NATIVE_FRAME_DERIVED_THIS_CONTEXT_OFFSET as usize)
                    .cast::<u32>()
                    .read(),
                if derived && frame_fid == fid {
                    result as u32
                } else {
                    0x3579
                }
            );
            assert_eq!(
                base.add(abi::NATIVE_FRAME_CONSTRUCT_RECEIVER_OFFSET as usize)
                    .cast::<u64>()
                    .read(),
                0x51a7_0123_4567_89ab
            );
        }
        assert_eq!(frame.header.function_id, frame_fid);
        assert!(frame.this_value.is_hole());
    }
}
