//! Immediate loads and canonical value materialization for x86-64 native tiers.
//!
//! # Contents
//! - Immediate words and artifact-aware symbol loads.
//! - Typed runtime-stub address loads into the call scratch register.
//! - Canonical Int32/Float64 boxing with explicit scratch ownership.
//!
//! # Invariants
//! - Address-bearing immediates carry their one relocation identity.
//! - Runtime entry loads use `r11`; the call ABI owner emits the call itself.
//! - Numeric boxing preserves the floating source, canonicalizes NaN, and
//!   writes only its destination/scratch GPs and flags; it never calls or allocates.
//!
//! # See also
//! - [`super::call_abi`] owns native C-call adaptation.
//! - [`crate::artifact::relocation`] owns portable relocation capture.

use dynasmrt::{DynasmApi, DynasmLabelApi, dynasm, x64::Assembler};
use otter_vm::native_abi as abi;

use crate::artifact::relocation::{RelocationCapture, RelocationTarget};

pub(crate) const NUMBER_TAG: u64 = (crate::entry::NUMBER_TAG_HI16 as u64) << 48;
pub(crate) const DOUBLE_OFFSET: u64 = (crate::entry::DOUBLE_OFFSET_HI16 as u64) << 48;
const CANONICAL_NAN: u64 = (crate::entry::CANONICAL_NAN_HI16 as u64) << 48;

/// Box the low signed Int32 payload. `destination` may alias `source` but
/// must differ from `scratch`; the scratch and arithmetic flags are clobbered.
pub(crate) fn emit_box_int32(ops: &mut Assembler, source: u8, destination: u8, scratch: u8) {
    debug_assert_ne!(destination, scratch);
    dynasm!(ops ; .arch x64 ; mov Rd(destination), Rd(source));
    emit_load_u64(ops, scratch, NUMBER_TAG);
    dynasm!(ops ; .arch x64 ; or Rq(destination), Rq(scratch));
}

/// Box an IEEE Float64 without changing its source XMM register. All NaNs
/// become the VM's canonical NaN; signed zero keeps its exact payload bits.
/// `destination` and `scratch` must differ; both GPs and flags are clobbered.
pub(crate) fn emit_box_double(ops: &mut Assembler, source: u8, destination: u8, scratch: u8) {
    debug_assert_ne!(destination, scratch);
    let ready = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64
        ; movq Rq(destination), Rx(source)
        ; ucomisd Rx(source), Rx(source)
        ; jnp =>ready
    );
    emit_load_u64(ops, destination, CANONICAL_NAN);
    dynasm!(ops ; .arch x64 ; =>ready);
    emit_load_u64(ops, scratch, DOUBLE_OFFSET);
    dynasm!(ops ; .arch x64 ; add Rq(destination), Rq(scratch));
}

pub(crate) fn emit_load_u64(ops: &mut Assembler, register: u8, value: u64) {
    if let Ok(value) = u32::try_from(value) {
        // A 32-bit register write clears its high word even when the
        // immediate's sign bit is set. MOV preserves prepared flags.
        // Dynamic Rd always emits a REX prefix, including for the low bank.
        // Static low-bank destinations avoid that otherwise redundant byte.
        match register {
            0 => dynasm!(ops ; .arch x64 ; mov eax, DWORD value as i32),
            1 => dynasm!(ops ; .arch x64 ; mov ecx, DWORD value as i32),
            2 => dynasm!(ops ; .arch x64 ; mov edx, DWORD value as i32),
            3 => dynasm!(ops ; .arch x64 ; mov ebx, DWORD value as i32),
            4 => dynasm!(ops ; .arch x64 ; mov esp, DWORD value as i32),
            5 => dynasm!(ops ; .arch x64 ; mov ebp, DWORD value as i32),
            6 => dynasm!(ops ; .arch x64 ; mov esi, DWORD value as i32),
            7 => dynasm!(ops ; .arch x64 ; mov edi, DWORD value as i32),
            _ => dynasm!(ops ; .arch x64 ; mov Rd(register), DWORD value as i32),
        }
    } else {
        dynasm!(ops ; .arch x64 ; mov Rq(register), QWORD value as i64);
    }
}

pub(crate) fn emit_load_symbol_u64(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    register: u8,
    value: u64,
    target: RelocationTarget,
) {
    let start = ops.offset().0;
    // Relocations validate and normalize the full ten-byte address move,
    // including an address that happens to fit in the low 32 bits.
    dynasm!(ops ; .arch x64 ; mov Rq(register), QWORD value as i64);
    relocations.record_x86_imm64(start, ops.offset().0, register, target);
}

pub(crate) fn emit_load_runtime_stub(
    ops: &mut Assembler,
    relocations: &mut RelocationCapture,
    address: u64,
    descriptor: abi::RuntimeStubDescriptor,
) {
    let start = ops.offset().0;
    dynasm!(ops ; .arch x64 ; mov r11, QWORD address as i64);
    relocations.record_x86_imm64(
        start,
        ops.offset().0,
        11,
        RelocationTarget::runtime_stub(descriptor),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_double_boxing_preserves_source_and_purifies_nan_for_each_scratch_pair() {
        for (destination, scratch) in [(0_u8, 11_u8), (9, 10), (10, 11), (11, 10)] {
            let mut ops = Assembler::new().unwrap();
            let entry = ops.offset();
            emit_box_double(&mut ops, 0, destination, scratch);
            dynasm!(ops ; .arch x64
                ; movq rdx, xmm0
                ; mov [rdi], rdx
                ; mov rax, Rq(destination)
                ; ret
            );
            let code = crate::CompiledCode::new(ops.finalize().unwrap(), entry);
            // SAFETY: the mapping accepts a System V f64 and one live output
            // word, returns a boxed word, and clobbers only caller-saved GPs.
            // It has no stack, allocation, or runtime-call boundary.
            let run: extern "sysv64" fn(f64, *mut u64) -> u64 =
                unsafe { std::mem::transmute(code.entry_ptr()) };
            for value in [
                -0.0,
                1.25,
                -2.5,
                f64::INFINITY,
                f64::NEG_INFINITY,
                f64::NAN,
                f64::from_bits(0x7ff0_0000_0000_0001),
                f64::from_bits(0xfff8_0012_3456_7890),
            ] {
                let mut source_bits = 0;
                assert_eq!(
                    run(value, &mut source_bits),
                    otter_vm::Value::number_f64(value).to_bits()
                );
                assert_eq!(
                    source_bits,
                    value.to_bits(),
                    "source XMM bits remain untouched"
                );
            }
        }
    }

    #[test]
    fn native_int32_boxing_discards_poisoned_high_word_with_aliased_destination() {
        for (destination, scratch) in [(0_u8, 11_u8), (7, 10)] {
            let mut ops = Assembler::new().unwrap();
            let entry = ops.offset();
            emit_box_int32(&mut ops, 7, destination, scratch);
            dynasm!(ops ; .arch x64 ; mov rax, Rq(destination) ; ret);
            let code = crate::CompiledCode::new(ops.finalize().unwrap(), entry);
            // SAFETY: this leaf System V fixture takes and returns u64 and
            // writes only volatile registers; the mapping outlives the call.
            let run: extern "sysv64" fn(u64) -> u64 =
                unsafe { std::mem::transmute(code.entry_ptr()) };
            for value in [i32::MIN, -731, 0, 913, i32::MAX] {
                let poisoned = 0xdead_beef_0000_0000 | u64::from(value as u32);
                assert_eq!(run(poisoned), otter_vm::Value::number_i32(value).to_bits());
            }
        }
    }
}
