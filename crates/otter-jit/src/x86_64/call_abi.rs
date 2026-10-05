//! Platform C calls around the engine's private x86-64 JavaScript convention.
//!
//! # Contents
//! - [`emit_runtime_call`] lowers a fixed VM descriptor's native C boundary.
//! - [`emit_variadic_call`] requires the exact physical word count at the site.
//! - [`emit_staged_call`] binds a generated request's exact caller return label.
//! - [`emit_c_entry`] admits a platform C tier entry to the shared private body.
//! - Executable tests exercise Microsoft aggregate returns and scalar leaves.
//!
//! # Invariants
//! - Prepared word arguments use rdi/rsi/rdx/rcx/r8/r9, then the caller's stack.
//! - The actual C call follows the target platform; Microsoft calls reserve
//!   shadow space, shift arguments for the hidden pair result, and normalize
//!   the VM-owned result back to rax/rdx without a second result carrier.
//! - The returned assembly coordinate is immediately after the actual CALL,
//!   before Microsoft pair reloads or temporary call-area cleanup.
//! - Private JavaScript call entries and their actual spans bypass this layer.
//! - RSP is aligned before a call and restored exactly afterwards. Published
//!   canonical root pointers are independent of the temporary C call area.
//! - External Microsoft entries preserve rdi/rsi and all of xmm6–xmm15 around
//!   the private body. Its frame already preserves the other nonvolatile GPs.
//!
//! # See also
//! - `otter_vm::native_abi::RuntimeStubDescriptor` owns signatures and domains.
//! - [`super::frame`] owns activation publication and canonical spill roots.
//! - Microsoft x64 calling convention: <https://learn.microsoft.com/en-us/cpp/build/x64-calling-convention>.

use dynasmrt::{AssemblyOffset, DynamicLabel, DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_vm::native_abi::{RuntimeStubDescriptor, RuntimeStubResultAbi, RuntimeStubSignature};

#[derive(Clone, Copy)]
enum Platform {
    SystemV,
    Microsoft,
}

impl Platform {
    const CURRENT: Self = if cfg!(target_os = "windows") {
        Self::Microsoft
    } else {
        Self::SystemV
    };
}

/// Physical arguments, distinct from a descriptor's semantic operand count.
#[derive(Clone, Copy)]
enum Arguments {
    Words(u8),
    Float64(u8),
}

/// Call the entry already in r11, using its authoritative fixed signature.
pub(crate) fn emit_runtime_call(
    ops: &mut Assembler,
    descriptor: RuntimeStubDescriptor,
) -> AssemblyOffset {
    emit_call(
        ops,
        Platform::CURRENT,
        descriptor_arguments(descriptor),
        descriptor.result_abi,
        None,
    )
}

fn descriptor_arguments(descriptor: RuntimeStubDescriptor) -> Arguments {
    use RuntimeStubSignature as S;
    match descriptor.signature {
        S::Poll1 | S::ExecutionEntry0 => Arguments::Words(1),
        S::ContextWords => Arguments::Words(1 + descriptor.argument_count),
        S::LeafValue2
        | S::MutatingLeafValue2
        | S::ReentrantValue2
        | S::ReentrantNamedLoad
        | S::ReentrantValueSpan
        | S::CommittedValue2 => Arguments::Words(3),
        S::MutatingLeafValue3 | S::ReentrantValue3 | S::ReentrantNamedStore => Arguments::Words(4),
        S::AllocValue3 => Arguments::Words(5),
        S::RouteThrow1 => Arguments::Words(2),
        S::Float64Leaf2 => Arguments::Float64(2),
        S::Float64ToWordLeaf1 => Arguments::Float64(1),
        S::Variadic => panic!("a variadic C call requires its physical word count"),
        S::JsCall => panic!("a private JavaScript call cannot use the platform C boundary"),
    }
}

/// Enter the existing execution trampoline, binding the request publisher's
/// label immediately after the physical CALL. Microsoft pair reloads and call
/// area cleanup follow this coordinate, just as the return-site table requires.
pub(crate) fn emit_staged_call(ops: &mut Assembler, return_label: DynamicLabel) -> AssemblyOffset {
    emit_call(
        ops,
        Platform::CURRENT,
        descriptor_arguments(otter_vm::native_abi::STUB_JIT_CALL),
        otter_vm::native_abi::STUB_JIT_CALL.result_abi,
        Some(return_label),
    )
}

/// Call a JIT-owned fixed physical site in the descriptor's Variadic family.
/// The count includes the context; semantic register counts are never inferred.
pub(crate) fn emit_variadic_call(
    ops: &mut Assembler,
    descriptor: RuntimeStubDescriptor,
    physical_words: u8,
) -> AssemblyOffset {
    assert_eq!(descriptor.signature, RuntimeStubSignature::Variadic);
    emit_call(
        ops,
        Platform::CURRENT,
        Arguments::Words(physical_words),
        descriptor.result_abi,
        None,
    )
}

fn emit_call(
    ops: &mut Assembler,
    platform: Platform,
    arguments: Arguments,
    result: RuntimeStubResultAbi,
    return_label: Option<DynamicLabel>,
) -> AssemblyOffset {
    let words = match arguments {
        Arguments::Words(words) => {
            assert!(
                (1..=7).contains(&words),
                "unsupported physical C word count"
            );
            words
        }
        Arguments::Float64(count) => {
            assert!((1..=2).contains(&count));
            assert_ne!(result, RuntimeStubResultAbi::NativePair);
            0
        }
    };
    if matches!(platform, Platform::SystemV) {
        dynasm!(ops ; .arch x64 ; call r11);
        if let Some(label) = return_label {
            dynasm!(ops ; .arch x64 ; =>label);
        }
        return ops.offset();
    }
    let pair = matches!(result, RuntimeStubResultAbi::NativePair);
    let parameters = u32::from(words) + u32::from(pair);
    let stack_words = parameters.saturating_sub(4);
    let result_offset = (32 + stack_words * 8).next_multiple_of(16);
    let bytes = if pair {
        result_offset + 16
    } else {
        (32 + stack_words * 8).next_multiple_of(16)
    };
    dynasm!(ops ; .arch x64 ; sub rsp, bytes as i32);
    // Store the outgoing stack arguments before overwriting any prepared
    // registers. The seventh prepared word remains in the caller's old area.
    let registers = [7_u8, 6, 2, 1, 8, 9];
    for index in 0..u32::from(words) {
        let position = index + u32::from(pair);
        if position < 4 {
            continue;
        }
        let offset = (32 + (position - 4) * 8) as i32;
        if let Some(&register) = registers.get(index as usize) {
            dynasm!(ops ; .arch x64 ; mov [rsp + offset], Rq(register));
        } else {
            dynasm!(ops ; .arch x64
                ; mov r10, [rsp + bytes as i32]
                ; mov [rsp + offset], r10
            );
        }
    }
    if words != 0 {
        if pair {
            // arg2 must leave rdx before arg0 takes it; arg3–arg6 were saved.
            if words >= 3 {
                dynasm!(ops ; .arch x64 ; mov r9, rdx);
            }
            if words >= 2 {
                dynasm!(ops ; .arch x64 ; mov r8, rsi);
            }
            dynasm!(ops ; .arch x64
                ; mov rdx, rdi
                ; lea rcx, [rsp + result_offset as i32]
            );
        } else {
            if words >= 4 {
                dynasm!(ops ; .arch x64 ; mov r9, rcx);
            }
            if words >= 3 {
                dynasm!(ops ; .arch x64 ; mov r8, rdx);
            }
            if words >= 2 {
                dynasm!(ops ; .arch x64 ; mov rdx, rsi);
            }
            dynasm!(ops ; .arch x64 ; mov rcx, rdi);
        }
    }
    dynasm!(ops ; .arch x64 ; call r11);
    let return_offset = ops.offset();
    if let Some(label) = return_label {
        dynasm!(ops ; .arch x64 ; =>label);
    }
    if pair {
        dynasm!(ops ; .arch x64
            ; mov rax, [rsp + result_offset as i32]
            ; mov rdx, [rsp + result_offset as i32 + 8]
        );
    }
    dynasm!(ops ; .arch x64 ; add rsp, bytes as i32);
    return_offset
}

/// Emit an external C tier-entry wrapper, then bind the private body's entry.
/// System V bodies already follow C entry/result rules, so emit no bytes there.
pub(crate) fn emit_c_entry(ops: &mut Assembler) {
    emit_entry(ops, Platform::CURRENT);
}

fn emit_entry(ops: &mut Assembler, platform: Platform) {
    if matches!(platform, Platform::SystemV) {
        return;
    }
    let body = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64
        ; push rbp
        ; mov rbp, rsp
        ; push rdi
        ; push rsi
        ; sub rsp, 176
        ; mov [rsp + 160], rcx
        ; mov rdi, rdx
    );
    for register in 6_u8..=15 {
        let offset = i32::from(register - 6) * 16;
        dynasm!(ops ; .arch x64 ; movdqu [rsp + offset], Rx(register));
    }
    dynasm!(ops ; .arch x64
        ; call =>body
        ; mov r10, [rsp + 160]
        ; mov [r10], rax
        ; mov [r10 + 8], rdx
        ; mov rax, r10
    );
    for register in 6_u8..=15 {
        let offset = i32::from(register - 6) * 16;
        dynasm!(ops ; .arch x64 ; movdqu Rx(register), [rsp + offset]);
    }
    dynasm!(ops ; .arch x64
        ; add rsp, 176
        ; pop rsi
        ; pop rdi
        ; pop rbp
        ; ret
        ; =>body
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use dynasmrt::AssemblyOffset;
    use otter_vm::{Value, native_abi::NativeResultPair};

    fn result(words: &[u64]) -> NativeResultPair {
        let payload = words
            .iter()
            .enumerate()
            .fold(0x7198_0037_u64, |value, (index, word)| {
                value.rotate_left(11) ^ word.wrapping_mul(index as u64 + 17)
            });
        if words[0] & 1 == 0 {
            NativeResultPair::success_bits(payload)
        } else {
            NativeResultPair::throw_value(Value::number_i32(payload as i32))
        }
    }

    macro_rules! entry {
        ($name:ident ($($argument:ident),+)) => {
            extern "win64" fn $name($($argument: u64),+) -> NativeResultPair {
                result(&[$($argument),+])
            }
        };
    }
    entry!(pair1(a));
    entry!(pair2(a, b));
    entry!(pair3(a, b, c));
    entry!(pair4(a, b, c, d));
    entry!(pair5(a, b, c, d, e));
    entry!(pair6(a, b, c, d, e, f));
    entry!(pair7(a, b, c, d, e, f, g));

    extern "win64" fn scalar7(a: u64, b: u64, c: u64, d: u64, e: u64, f: u64, g: u64) -> u64 {
        result(&[a, b, c, d, e, f, g]).payload_bits()
    }

    /// The harness has independent result canaries and records the prepared
    /// call-area pointer on both sides; the C boundary must restore it exactly.
    fn run_words(entry: usize, words: &[u64], result_abi: RuntimeStubResultAbi) -> [u64; 6] {
        let mut ops = Assembler::new().unwrap();
        let entry_offset = ops.offset();
        dynasm!(ops ; .arch x64
            ; push rbp
            ; mov rbp, rsp
            ; push rbx
            ; sub rsp, 8
            ; mov rbx, rdi
        );
        if words.len() == 7 {
            dynasm!(ops ; .arch x64 ; sub rsp, 16 ; mov rax, QWORD words[6] as i64 ; mov [rsp], rax);
        }
        for (&register, &word) in [7_u8, 6, 2, 1, 8, 9].iter().zip(words) {
            dynasm!(ops ; .arch x64 ; mov Rq(register), QWORD word as i64);
        }
        dynasm!(ops ; .arch x64 ; mov [rbx + 32], rsp ; mov r11, QWORD entry as i64);
        emit_call(
            &mut ops,
            Platform::Microsoft,
            Arguments::Words(words.len() as u8),
            result_abi,
            None,
        );
        dynasm!(ops ; .arch x64
            ; mov [rbx + 8], rax
            ; mov [rbx + 16], rdx
            ; mov [rbx + 40], rsp
        );
        if words.len() == 7 {
            dynasm!(ops ; .arch x64 ; add rsp, 16);
        }
        dynasm!(ops ; .arch x64 ; add rsp, 8 ; pop rbx ; pop rbp ; ret);
        let code = crate::CompiledCode::new(ops.finalize().unwrap(), entry_offset);
        // SAFETY: the harness follows System V, preserves its nonvolatile GPs,
        // keeps both owned C entry and mapping alive, and only writes six words.
        let run: extern "sysv64" fn(*mut u64) = unsafe { std::mem::transmute(code.entry_ptr()) };
        let mut observation = [0x1738_fff0_a3c1_99d7; 6];
        run(observation.as_mut_ptr());
        assert_eq!(observation[0], 0x1738_fff0_a3c1_99d7);
        assert_eq!(observation[3], 0x1738_fff0_a3c1_99d7);
        assert_eq!(
            observation[4], observation[5],
            "exact C call-area restoration"
        );
        assert_eq!(observation[4] & 15, 0, "prepared call stack is aligned");
        observation
    }

    #[test]
    fn microsoft_hidden_pair_returns_preserve_every_physical_argument_and_status() {
        let entries = [
            pair1 as *const () as usize,
            pair2 as *const () as usize,
            pair3 as *const () as usize,
            pair4 as *const () as usize,
            pair5 as *const () as usize,
            pair6 as *const () as usize,
            pair7 as *const () as usize,
        ];
        for first in [0x7198_0012_ffff_0000, 0x7198_0012_ffff_0001] {
            let words = [
                first,
                0x8fff_7134_c002_1107,
                u64::MAX,
                0x8000_0000_0000_0000,
                0xa317_57ff_9912_2313,
                0x19fe_c012_1700_9418,
                0xe799_8613_5137_0177,
            ];
            for (index, entry) in entries.into_iter().enumerate() {
                let words = &words[..index + 1];
                let expected = result(words);
                let actual = run_words(entry, words, RuntimeStubResultAbi::NativePair);
                assert_eq!(
                    actual[1],
                    expected.payload_bits(),
                    "physical words {}",
                    words.len()
                );
                assert_eq!(
                    actual[2],
                    expected
                        .validate(otter_vm::native_abi::NativeResultDomain::Committed)
                        .unwrap() as u64,
                    "physical words {}",
                    words.len()
                );
            }
        }
    }

    #[test]
    fn microsoft_scalar_call_keeps_unshifted_register_and_stack_arguments() {
        let words = [2, 17, 0x8000_0000_0000_0000, u64::MAX, 171, 37, 91];
        let actual = run_words(
            scalar7 as *const () as usize,
            &words,
            RuntimeStubResultAbi::ValueWord,
        );
        assert_eq!(actual[1], result(&words).payload_bits());
    }

    #[unsafe(naked)]
    extern "sysv64" fn observe_sysv_return(_observed: *mut [u64; 2]) -> NativeResultPair {
        core::arch::naked_asm!(
            "mov r10, [rsp]",
            "mov [rdi], r10",
            "mov eax, 917",
            "xor edx, edx",
            "ret",
        );
    }

    #[unsafe(naked)]
    extern "win64" fn observe_microsoft_return(_observed: *mut [u64; 2]) -> NativeResultPair {
        core::arch::naked_asm!(
            "mov r10, [rsp]",
            "mov [rdx], r10",
            "mov qword ptr [rcx], 917",
            "mov qword ptr [rcx + 8], 0",
            "mov rax, rcx",
            "ret",
        );
    }

    #[test]
    fn staged_return_label_precedes_microsoft_pair_reload_and_matches_hardware() {
        for (platform, target) in [
            (Platform::SystemV, observe_sysv_return as *const () as usize),
            (
                Platform::Microsoft,
                observe_microsoft_return as *const () as usize,
            ),
        ] {
            let mut ops = Assembler::new().unwrap();
            let entry = ops.offset();
            let label = ops.new_dynamic_label();
            dynasm!(ops ; .arch x64
                ; push rbp
                ; mov rbp, rsp
                ; lea r10, [=>label]
                ; mov [rdi + 8], r10
                ; mov r11, QWORD target as i64
            );
            let returned = emit_call(
                &mut ops,
                platform,
                Arguments::Words(1),
                RuntimeStubResultAbi::NativePair,
                Some(label),
            );
            if matches!(platform, Platform::Microsoft) {
                assert!(
                    ops.offset().0 > returned.0,
                    "reloads follow the actual CALL"
                );
            } else {
                assert_eq!(ops.offset(), returned);
            }
            dynasm!(ops ; .arch x64 ; pop rbp ; ret);
            let code = crate::CompiledCode::new(ops.finalize().unwrap(), entry);
            // SAFETY: the finalized mapping outlives every use of its entry.
            let entry_address = unsafe { code.entry_ptr() };
            let expected = entry_address as usize as u64 + (returned.0 - entry.0) as u64;
            let run: extern "sysv64" fn(*mut [u64; 2]) -> NativeResultPair =
                unsafe { std::mem::transmute(entry_address) };
            let mut observed = [0_u64; 2];
            // SAFETY: the wrapper owns both ABI call areas and two writable words.
            let result = run(&mut observed);
            assert_eq!(result.payload_bits(), 917);
            assert_eq!(
                result.validate(otter_vm::native_abi::NativeResultDomain::Execution),
                Some(otter_vm::native_abi::NativeResultStatus::Success)
            );
            assert_eq!(observed, [expected, expected]);
        }
    }

    extern "win64" fn multiply(left: f64, right: f64) -> f64 {
        left * right
    }

    extern "win64" fn float_bits(value: f64) -> u64 {
        value.to_bits()
    }

    #[test]
    fn microsoft_float_leaves_keep_fp_positions_and_exact_result_bits() {
        let mut ops = Assembler::new().unwrap();
        let start = ops.offset();
        dynasm!(ops ; .arch x64 ; push rbp ; mov rbp, rsp ; mov r11, QWORD multiply as *const () as usize as i64);
        emit_call(
            &mut ops,
            Platform::Microsoft,
            Arguments::Float64(2),
            RuntimeStubResultAbi::Float64,
            None,
        );
        dynasm!(ops ; .arch x64 ; pop rbp ; ret);
        let code = crate::CompiledCode::new(ops.finalize().unwrap(), start);
        // SAFETY: both ABIs place these two scalar floats in xmm0/xmm1 and
        // return xmm0; the emitted C boundary owns its aligned shadow area.
        let run: extern "sysv64" fn(f64, f64) -> f64 =
            unsafe { std::mem::transmute(code.entry_ptr()) };
        for (left, right) in [
            (0.0, -1.0),
            (-0.0, 1.0),
            (3.5, -1.25),
            (f64::INFINITY, -1.0),
        ] {
            assert_eq!(run(left, right).to_bits(), multiply(left, right).to_bits());
        }
        let mut ops = Assembler::new().unwrap();
        let start = ops.offset();
        dynasm!(ops ; .arch x64 ; push rbp ; mov rbp, rsp ; mov r11, QWORD float_bits as *const () as usize as i64);
        emit_call(
            &mut ops,
            Platform::Microsoft,
            Arguments::Float64(1),
            RuntimeStubResultAbi::ValueWord,
            None,
        );
        dynasm!(ops ; .arch x64 ; pop rbp ; ret);
        let code = crate::CompiledCode::new(ops.finalize().unwrap(), start);
        // SAFETY: one scalar FP input and one word result, mapping kept alive.
        let run: extern "sysv64" fn(f64) -> u64 = unsafe { std::mem::transmute(code.entry_ptr()) };
        for bits in [
            0,
            (-0.0_f64).to_bits(),
            0x7ff8_0000_0000_0731,
            f64::NEG_INFINITY.to_bits(),
        ] {
            assert_eq!(run(f64::from_bits(bits)), bits);
        }
    }

    #[test]
    fn microsoft_external_entry_preserves_full_nonvolatile_fp_and_integer_state() {
        let mut ops = Assembler::new().unwrap();
        let start = ops.offset();
        emit_entry(&mut ops, Platform::Microsoft);
        // The private body is permitted to clobber rdi/rsi and every FP
        // register. Preserve the other nonvolatile GPs, as the real frame does.
        dynasm!(ops ; .arch x64
            ; mov rax, [rdi]
            ; mov edx, 2
            ; mov rdi, QWORD -1
            ; mov rsi, QWORD -1
        );
        for register in 6_u8..=15 {
            dynasm!(ops ; .arch x64 ; pxor Rx(register), Rx(register));
        }
        dynasm!(ops ; .arch x64 ; ret);
        let body = crate::CompiledCode::new(ops.finalize().unwrap(), start);

        let mut ops = Assembler::new().unwrap();
        let start = ops.offset();
        dynasm!(ops ; .arch x64
            ; push rbp
            ; mov rbp, rsp
            ; push rbx
            ; push r12
            ; sub rsp, 48
            ; mov rbx, rdi
            ; mov r12, rsi
            ; mov rdi, QWORD 0x1738_37aa_fff0_9912_u64 as i64
            ; mov rsi, QWORD 0xf799_11fe_8173_0731_u64 as i64
        );
        let mut expected_fp = vec![];
        for register in 6_u8..=15 {
            let low = 0x8000_0000_0000_0731_u64 + u64::from(register);
            let high = 0x7ff8_0000_0000_0137_u64 + u64::from(register);
            expected_fp.extend([low, high]);
            dynasm!(ops ; .arch x64
                ; mov rax, QWORD low as i64
                ; movq Rx(register), rax
                ; mov rax, QWORD high as i64
                ; movq xmm0, rax
                ; punpcklqdq Rx(register), xmm0
            );
        }
        // SAFETY: the private body mapping remains owned by this test until
        // the probe returns; its entry implements the exact prepared ABI.
        let body_entry = unsafe { body.entry_ptr() } as usize;
        dynasm!(ops ; .arch x64
            ; lea rcx, [rsp + 32]
            ; mov rdx, r12
            ; mov r11, QWORD body_entry as i64
            ; call r11
            ; mov [rbx + 192], rax
            ; lea r10, [rsp + 32]
            ; mov [rbx + 200], r10
            ; mov r10, [rsp + 32]
            ; mov [rbx], r10
            ; mov r10, [rsp + 40]
            ; mov [rbx + 8], r10
            ; mov [rbx + 16], rdi
            ; mov [rbx + 24], rsi
        );
        for register in 6_u8..=15 {
            let offset = 32 + i32::from(register - 6) * 16;
            dynasm!(ops ; .arch x64 ; movdqu [rbx + offset], Rx(register));
        }
        dynasm!(ops ; .arch x64 ; add rsp, 48 ; pop r12 ; pop rbx ; pop rbp ; ret);
        let probe = crate::CompiledCode::new(ops.finalize().unwrap(), start);
        // SAFETY: the probe follows System V and writes exactly 26 owned
        // words. The second pointer is read only by the private body; both
        // mappings and the prepared word remain alive through the call.
        let run: extern "sysv64" fn(*mut u64, *const u64) =
            unsafe { std::mem::transmute(probe.entry_ptr()) };
        let payload = NativeResultPair::success(Value::number_i32(731)).payload_bits();
        let mut observed = [0_u64; 26];
        run(observed.as_mut_ptr(), &payload);
        assert_eq!(observed[0], payload);
        assert_eq!(
            observed[1], 2,
            "private pair becomes the external aggregate"
        );
        assert_eq!(observed[2], 0x1738_37aa_fff0_9912);
        assert_eq!(observed[3], 0xf799_11fe_8173_0731);
        assert_eq!(
            &observed[4..24],
            expected_fp.as_slice(),
            "all 128 bits of each nonvolatile FP register survive"
        );
        assert_eq!(
            observed[24], observed[25],
            "C returns its exact hidden result pointer"
        );
    }

    #[test]
    fn system_v_external_entry_has_no_wrapper_overhead() {
        let mut ops = Assembler::new().unwrap();
        emit_entry(&mut ops, Platform::SystemV);
        assert_eq!(ops.offset(), AssemblyOffset(0));
    }
}
