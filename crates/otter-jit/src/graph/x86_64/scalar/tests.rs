//! Executable tests of the actual x86 scalar encoders and their exit operands.
//!
//! # Contents
//! - Aliased integer results, division traps and complete eager input words.
//! - CL/count masking and two-address floating operand preservation.
//! - Ordered/unordered predicates, exact conversions and canonical Number words.
//!
//! # Invariants
//! - Fixtures call production encoders, never a simulated instruction model.
//! - Both successful results and pre-effect failure inputs are independently checked.
//! - Mappings and owned observation windows outlive every native invocation.

use super::*;
use super::{arithmetic as arith, conversions as convert};

mod leaves;

#[derive(Clone, Copy)]
struct Exits {
    overflow: DynamicLabel,
    minus_zero: DynamicLabel,
    lost: DynamicLabel,
}

fn integer_fixture(dst: u8, emit: impl FnOnce(&mut Assembler, Exits)) -> crate::CompiledCode {
    let mut ops = Assembler::new().unwrap();
    let entry = ops.offset();
    let exits = Exits {
        overflow: ops.new_dynamic_label(),
        minus_zero: ops.new_dynamic_label(),
        lost: ops.new_dynamic_label(),
    };
    let finish = ops.new_dynamic_label();
    dynasm!(ops ; .arch x64 ; mov r8, rdx);
    emit(&mut ops, exits);
    dynasm!(ops ; .arch x64
        ; mov [r8], Rq(dst) ; xor eax, eax ; jmp =>finish
        ; =>exits.overflow ; mov eax, 1 ; jmp =>finish
        ; =>exits.minus_zero ; mov eax, 2 ; jmp =>finish
        ; =>exits.lost ; mov eax, 3
        ; =>finish ; mov [r8 + 8], rdi ; mov [r8 + 16], rsi ; ret
    );
    crate::CompiledCode::new(ops.finalize().unwrap(), entry)
}

fn run_integer(code: &crate::CompiledCode, a: i32, b: i32) -> (u64, [u64; 4]) {
    let a = 0xface_9137_0000_0000 | u64::from(a as u32);
    let b = 0xdead_7319_0000_0000 | u64::from(b as u32);
    let mut observation = [0xa17f_0037_cafe_7139; 4];
    // SAFETY: the pure encoder follows System V, modifies volatile registers
    // only, and writes three words of this owned four-word observation window.
    let entry: extern "sysv64" fn(u64, u64, *mut u64) -> u64 =
        unsafe { std::mem::transmute(code.entry_ptr()) };
    let status = entry(a, b, observation.as_mut_ptr());
    assert_eq!(observation[3], 0xa17f_0037_cafe_7139);
    if status != 0 {
        assert_eq!(
            &observation[1..3],
            &[a, b],
            "eager exit retains complete original words"
        );
    }
    (status, observation)
}

#[test]
fn checked_integer_results_commit_after_overflow_and_negative_zero_guards() {
    let values = [i32::MIN, -46341, -1, 0, 1, 7, 46341, i32::MAX];
    for dst in [0_u8, 7, 6] {
        for add in [true, false] {
            for constant in [None, Some(i32::MIN), Some(-1), Some(0), Some(i32::MAX)] {
                let code = integer_fixture(dst, |ops, exits| {
                    arith::emit_add_sub(
                        ops,
                        add,
                        7,
                        constant.map_or(Int32Operand::Register(6), Int32Operand::Constant),
                        dst,
                        exits.overflow,
                    );
                });
                for a in values {
                    for supplied_b in values {
                        let b = constant.unwrap_or(supplied_b);
                        let expected = if add {
                            a.checked_add(b)
                        } else {
                            a.checked_sub(b)
                        };
                        let (status, observed) = run_integer(&code, a, supplied_b);
                        assert_eq!(
                            status,
                            u64::from(expected.is_none()),
                            "{a} {add} {b} -> GP{dst}"
                        );
                        if let Some(value) = expected {
                            assert_eq!(observed[0], u64::from(value as u32));
                        }
                    }
                }
            }
        }
        let code = integer_fixture(dst, |ops, exits| {
            arith::emit_mul(ops, 7, 6, dst, exits.overflow, exits.minus_zero)
        });
        for a in values {
            for b in values {
                let expected = a.checked_mul(b);
                let expected_status = if expected.is_none() {
                    1
                } else if expected == Some(0) && (a < 0 || b < 0) {
                    2
                } else {
                    0
                };
                let (status, observed) = run_integer(&code, a, b);
                assert_eq!(status, expected_status, "{a} * {b} -> GP{dst}");
                if status == 0 {
                    assert_eq!(observed[0], u64::from(expected.unwrap() as u32));
                }
            }
        }
        let code = integer_fixture(dst, |ops, exits| {
            arith::emit_negate(ops, 7, dst, exits.minus_zero, exits.overflow)
        });
        for a in values {
            let (status, observed) = run_integer(&code, a, 913);
            assert_eq!(
                status,
                if a == 0 {
                    2
                } else if a == i32::MIN {
                    1
                } else {
                    0
                }
            );
            if status == 0 {
                assert_eq!(observed[0], u64::from((-a) as u32));
            }
        }
    }
}

#[test]
fn division_prevents_machine_traps_and_retains_exact_eager_operands() {
    let values = [i32::MIN, -17, -4, -1, 0, 1, 4, 17, i32::MAX];
    for remainder in [false, true] {
        let dst = if remainder { 2 } else { 0 };
        let code = integer_fixture(dst, |ops, exits| {
            arith::emit_div_mod(
                ops,
                remainder,
                7,
                6,
                exits.lost,
                exits.minus_zero,
                if remainder {
                    exits.minus_zero
                } else {
                    exits.overflow
                },
            )
        });
        for a in values {
            for b in values {
                let (status, observed) = run_integer(&code, a, b);
                let (expected_status, expected) = if b == 0 {
                    (3, 0)
                } else if a == i32::MIN && b == -1 {
                    (if remainder { 2 } else { 1 }, 0)
                } else if remainder {
                    let value = a % b;
                    (if value == 0 && a < 0 { 2 } else { 0 }, value)
                } else if a == 0 && b < 0 {
                    (2, 0)
                } else if a % b != 0 {
                    (3, 0)
                } else {
                    (0, a / b)
                };
                assert_eq!(
                    status,
                    expected_status,
                    "{a} {} {b}",
                    if remainder { "%" } else { "/" }
                );
                if status == 0 {
                    assert_eq!(observed[0], u64::from(expected as u32));
                }
            }
        }
    }
}

#[test]
fn shifts_consume_cl_before_aliased_result_and_mask_constant_counts() {
    for kind in [
        Kind::Int32ShiftLeft,
        Kind::Int32ShiftRight,
        Kind::Int32ShiftRightLogical,
    ] {
        for dst in [0_u8, 7, 1] {
            for constant in [None, Some(0), Some(32), Some(-1), Some(63)] {
                let code = integer_fixture(dst, |ops, exits| {
                    dynasm!(ops ; .arch x64 ; mov rcx, rsi);
                    arith::emit_shift(
                        ops,
                        &kind,
                        7,
                        constant.map_or(Int32Operand::Register(1), Int32Operand::Constant),
                    );
                    if kind == Kind::Int32ShiftRightLogical {
                        dynasm!(ops ; .arch x64 ; test r10d, r10d ; js =>exits.lost);
                    }
                    dynasm!(ops ; .arch x64 ; mov Rd(dst), r10d);
                });
                for a in [i32::MIN, -1, 0, 7, i32::MAX] {
                    for supplied_b in [-65, -1, 0, 1, 31, 32, 65] {
                        let count = (constant.unwrap_or(supplied_b) & 31) as u32;
                        let value = match kind {
                            Kind::Int32ShiftLeft => a.wrapping_shl(count) as u32,
                            Kind::Int32ShiftRight => a.wrapping_shr(count) as u32,
                            _ => (a as u32).wrapping_shr(count),
                        };
                        let expected_status =
                            if kind == Kind::Int32ShiftRightLogical && value > i32::MAX as u32 {
                                3
                            } else {
                                0
                            };
                        let (status, observed) = run_integer(&code, a, supplied_b);
                        assert_eq!(status, expected_status);
                        if status == 0 {
                            assert_eq!(observed[0], u64::from(value));
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn scalar_float_comparisons_obey_unordered_and_signed_zero_conditions() {
    for condition in [
        Condition::Equal,
        Condition::NotEqual,
        Condition::Less,
        Condition::LessEqual,
        Condition::Greater,
        Condition::GreaterEqual,
    ] {
        let mut ops = Assembler::new().unwrap();
        let start = ops.offset();
        dynasm!(ops ; .arch x64 ; ucomisd xmm0, xmm1);
        emit_cset_bool(&mut ops, 0, condition, true);
        dynasm!(ops ; .arch x64 ; ret);
        let code = crate::CompiledCode::new(ops.finalize().unwrap(), start);
        // SAFETY: two scalar System V float inputs, one word result, no stack
        // changes or calls; the mapping outlives every invocation.
        let run: extern "sysv64" fn(f64, f64) -> u64 =
            unsafe { std::mem::transmute(code.entry_ptr()) };
        for a in [
            f64::NEG_INFINITY,
            -7.5,
            -0.0,
            0.0,
            7.5,
            f64::INFINITY,
            f64::NAN,
        ] {
            for b in [
                f64::NEG_INFINITY,
                -7.5,
                -0.0,
                0.0,
                7.5,
                f64::INFINITY,
                f64::NAN,
            ] {
                let expected = match condition {
                    Condition::Equal => a == b,
                    Condition::NotEqual => a != b,
                    Condition::Less => a < b,
                    Condition::LessEqual => a <= b,
                    Condition::Greater => a > b,
                    Condition::GreaterEqual => a >= b,
                };
                assert_eq!(
                    run(a, b),
                    if expected {
                        tag::VALUE_TRUE
                    } else {
                        tag::VALUE_FALSE
                    },
                    "{condition:?}: {a:?}, {b:?}"
                );
            }
        }
    }
}

#[test]
fn float_two_address_results_preserve_rhs_and_nan_zero_semantics() {
    for kind in [
        Kind::Float64Add,
        Kind::Float64Sub,
        Kind::Float64Mul,
        Kind::Float64Div,
    ] {
        for dst in [0_u8, 1, 7] {
            let mut ops = Assembler::new().unwrap();
            let start = ops.offset();
            arith::emit_float_binary(&mut ops, &kind, 0, 1, dst);
            dynasm!(ops ; .arch x64 ; movq rax, Rx(dst) ; ret);
            let code = crate::CompiledCode::new(ops.finalize().unwrap(), start);
            // SAFETY: this pure System V float fixture writes only volatile
            // XMM registers/RAX and retains its executable mapping.
            let run: extern "sysv64" fn(f64, f64) -> u64 =
                unsafe { std::mem::transmute(code.entry_ptr()) };
            for a in [-0.0, 0.0, -3.5, 13.25, f64::INFINITY, f64::NAN] {
                for b in [-0.0, 0.0, -3.5, 13.25, f64::INFINITY, f64::NAN] {
                    let expected = match kind {
                        Kind::Float64Add => a + b,
                        Kind::Float64Sub => a - b,
                        Kind::Float64Mul => a * b,
                        _ => a / b,
                    };
                    let actual = f64::from_bits(run(a, b));
                    if expected.is_nan() {
                        assert!(actual.is_nan());
                    } else {
                        assert_eq!(actual.to_bits(), expected.to_bits(), "{kind:?} -> XMM{dst}");
                    }
                }
            }
        }
    }
}

#[test]
fn exact_float_conversions_reject_nan_range_fraction_and_distinguish_index_negative_zero() {
    for index in [false, true] {
        let mut ops = Assembler::new().unwrap();
        let start = ops.offset();
        let exit = ops.new_dynamic_label();
        let zero = ops.new_dynamic_label();
        let finish = ops.new_dynamic_label();
        // Keep the allocated destination's original full word as a canary.
        dynasm!(ops ; .arch x64 ; mov rdx, QWORD 0x1738_fffe_7319_9137_u64 as i64);
        convert::emit_checked_float_to_int(&mut ops, 0, 2, exit, (!index).then_some(zero));
        dynasm!(ops ; .arch x64
            ; xor eax, eax ; jmp =>finish ; =>exit ; mov eax, 1 ; jmp =>finish
            ; =>zero ; mov eax, 2 ; =>finish ; mov [rdi], rdx ; movq rsi, xmm0 ; mov [rdi + 8], rsi ; ret
        );
        let code = crate::CompiledCode::new(ops.finalize().unwrap(), start);
        // SAFETY: one float and a pointer to two owned observation words,
        // volatile register writes only; this fixture makes no calls.
        let run: extern "sysv64" fn(f64, *mut u64) -> u64 =
            unsafe { std::mem::transmute(code.entry_ptr()) };
        for value in [
            f64::NEG_INFINITY,
            i32::MIN as f64 - 1.0,
            i32::MIN as f64,
            -1.5,
            -0.0,
            0.0,
            17.0,
            i32::MAX as f64,
            i32::MAX as f64 + 1.0,
            f64::INFINITY,
            f64::NAN,
        ] {
            let mut observed = [0_u64; 2];
            let status = run(value, observed.as_mut_ptr());
            let exact = value.is_finite()
                && value.fract() == 0.0
                && value >= i32::MIN as f64
                && value <= i32::MAX as f64;
            let expected_status = if !exact {
                1
            } else if !index && value.to_bits() == (-0.0_f64).to_bits() {
                2
            } else {
                0
            };
            assert_eq!(status, expected_status, "index={index}: {value:?}");
            assert_eq!(
                observed[1],
                value.to_bits(),
                "source survives precision and negative-zero guards"
            );
            assert_eq!(
                observed[0],
                if status == 0 {
                    u64::from(value as i32 as u32)
                } else {
                    0x1738_fffe_7319_9137
                }
            );
        }
    }
}

#[test]
fn graph_number_boxing_is_canonical_at_integer_edges_nan_and_signed_zero() {
    let mut ops = Assembler::new().unwrap();
    let start = ops.offset();
    convert::emit_box_number(&mut ops, 0, 0);
    dynasm!(ops ; .arch x64 ; movq rdx, xmm0 ; mov [rdi], rdx ; ret);
    let code = crate::CompiledCode::new(ops.finalize().unwrap(), start);
    // SAFETY: scalar float input plus one owned observation word, one word
    // result and only volatile scratch; the mapping remains owned here.
    let run: extern "sysv64" fn(f64, *mut u64) -> u64 =
        unsafe { std::mem::transmute(code.entry_ptr()) };
    for value in [
        i32::MIN as f64 - 1.0,
        i32::MIN as f64,
        -731.0,
        -0.0,
        0.0,
        17.0,
        1.5,
        i32::MAX as f64,
        i32::MAX as f64 + 1.0,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::from_bits(0xfff8_0037_cafe_7319),
    ] {
        let mut source = 0;
        let exact_integer = value.is_finite()
            && value.fract() == 0.0
            && value >= i32::MIN as f64
            && value <= i32::MAX as f64
            && value.to_bits() != (-0.0_f64).to_bits();
        let expected = if exact_integer {
            tag::box_int32(value as i32)
        } else {
            tag::box_double(if value.is_nan() {
                tag::CANONICAL_NAN
            } else {
                value.to_bits()
            })
        };
        assert_eq!(run(value, &mut source), expected, "{value:?}");
        assert_eq!(source, value.to_bits());
    }
}

#[test]
fn uint32_shift_converts_the_full_unsigned_domain_without_signed_truncation() {
    for constant in [None, Some(0), Some(32), Some(-1), Some(7)] {
        let mut ops = Assembler::new().unwrap();
        let start = ops.offset();
        dynasm!(ops ; .arch x64 ; mov rcx, rsi);
        arith::emit_shift(
            &mut ops,
            &Kind::Uint32ShiftRightToFloat64,
            7,
            constant.map_or(Int32Operand::Register(1), Int32Operand::Constant),
        );
        dynasm!(ops ; .arch x64 ; cvtsi2sd xmm0, r10 ; ret);
        let code = crate::CompiledCode::new(ops.finalize().unwrap(), start);
        // SAFETY: pure System V word inputs, scalar FP output and volatile
        // scratch only; the code mapping remains alive throughout the loop.
        let run: extern "sysv64" fn(u64, u64) -> f64 =
            unsafe { std::mem::transmute(code.entry_ptr()) };
        for value in [0_u32, 1, i32::MAX as u32, 0x8000_0000, u32::MAX] {
            for supplied in [-65_i32, -1, 0, 7, 31, 32, 65] {
                let count = (constant.unwrap_or(supplied) & 31) as u32;
                assert_eq!(
                    run(0xffff_7319_0000_0000 | u64::from(value), supplied as u64),
                    f64::from(value >> count)
                );
            }
        }
    }
}

#[test]
fn bitwise_rhs_alias_and_signed_integer_compare_match_full_word_conditions() {
    let values = [i32::MIN, -1, 0, 1738, i32::MAX];
    for kind in [Kind::Int32BitAnd, Kind::Int32BitOr, Kind::Int32BitXor] {
        for dst in [0_u8, 6, 7] {
            for constant in [None, Some(i32::MIN), Some(-1), Some(0), Some(i32::MAX)] {
                let code = integer_fixture(dst, |ops, _| {
                    arith::emit_bitwise(
                        ops,
                        &kind,
                        7,
                        constant.map_or(Int32Operand::Register(6), Int32Operand::Constant),
                        dst,
                    );
                });
                for a in values {
                    for supplied in values {
                        let b = constant.unwrap_or(supplied);
                        let expected = match kind {
                            Kind::Int32BitAnd => a & b,
                            Kind::Int32BitOr => a | b,
                            _ => a ^ b,
                        };
                        let (status, observed) = run_integer(&code, a, supplied);
                        assert_eq!(status, 0);
                        assert_eq!(observed[0], u64::from(expected as u32));
                    }
                }
            }
        }
    }
    for condition in [
        Condition::Equal,
        Condition::NotEqual,
        Condition::Less,
        Condition::LessEqual,
        Condition::Greater,
        Condition::GreaterEqual,
    ] {
        for constant in [None, Some(i32::MIN), Some(-1), Some(i32::MAX)] {
            let code = integer_fixture(0, |ops, _| {
                emit_int32_compare(
                    ops,
                    7,
                    constant.map_or(Int32Operand::Register(6), Int32Operand::Constant),
                );
                emit_cset_bool(ops, 0, condition, false);
            });
            for a in values {
                for supplied in values {
                    let b = constant.unwrap_or(supplied);
                    let expected = match condition {
                        Condition::Equal => a == b,
                        Condition::NotEqual => a != b,
                        Condition::Less => a < b,
                        Condition::LessEqual => a <= b,
                        Condition::Greater => a > b,
                        Condition::GreaterEqual => a >= b,
                    };
                    let (status, observed) = run_integer(&code, a, supplied);
                    assert_eq!(status, 0);
                    assert_eq!(
                        observed[0],
                        if expected {
                            tag::VALUE_TRUE
                        } else {
                            tag::VALUE_FALSE
                        }
                    );
                }
            }
        }
    }
}

#[test]
fn tagged_number_conversion_preserves_source_and_rejects_non_numbers_before_result() {
    let mut ops = Assembler::new().unwrap();
    let start = ops.offset();
    let exit = ops.new_dynamic_label();
    let done = ops.new_dynamic_label();
    crate::x86_64::values::emit_load_u64(&mut ops, 10, 1.25_f64.to_bits());
    dynasm!(ops ; .arch x64 ; movq xmm0, r10);
    convert::emit_tagged_number_to_float(&mut ops, 7, 0, exit);
    dynasm!(ops ; .arch x64
        ; xor eax, eax ; jmp =>done ; =>exit ; mov eax, 1
        ; =>done ; movsd [rsi], xmm0 ; mov [rsi + 8], rdi ; ret
    );
    let code = crate::CompiledCode::new(ops.finalize().unwrap(), start);
    // SAFETY: one tagged word and a two-word owned observation span, with
    // no calls, allocation or heap dereference; only volatile words change.
    let run: extern "sysv64" fn(u64, *mut u64) -> u64 =
        unsafe { std::mem::transmute(code.entry_ptr()) };
    let values = [
        tag::box_int32(i32::MIN),
        tag::box_int32(-1),
        tag::box_int32(0),
        tag::box_int32(i32::MAX),
        tag::box_double((-0.0_f64).to_bits()),
        tag::box_double(0.0_f64.to_bits()),
        tag::box_double(1.5_f64.to_bits()),
        tag::box_double(f64::INFINITY.to_bits()),
        tag::box_double(tag::CANONICAL_NAN),
        tag::VALUE_NULL,
        tag::VALUE_FALSE,
        tag::VALUE_UNDEFINED,
        tag::box_function_id(7319),
    ];
    for value in values {
        let mut observed = [0_u64; 2];
        let status = run(value, observed.as_mut_ptr());
        assert_eq!(observed[1], value);
        assert_eq!(status, u64::from(!tag::is_number_bits(value)));
        let expected = if tag::is_int32_bits(value) {
            f64::from(value as i32).to_bits()
        } else if tag::is_number_bits(value) {
            tag::unbox_double(value)
        } else {
            1.25_f64.to_bits()
        };
        assert_eq!(observed[0], expected);
    }
}
