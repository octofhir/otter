//! Executable numeric-domain feedback from actual Template operations.
//!
//! # Contents
//! - Binary, update, negate and relational paths observe Double-encoded inputs.
//!
//! # Invariants
//! - Inputs retain their encoded words and results retain JavaScript semantics.
//! - Exact Int32 Doubles stay narrow; fractions, signed zero and overflow widen.
//! - Feedback comes from the production operation, not a standalone classifier.
//!
//! # See also
//! - `super` owns the actual arithmetic and feedback machine emission.

use super::*;
use otter_vm::{
    Value,
    jit_feedback::{ARITH_FLOAT64, ARITH_INT32},
};

#[derive(Clone, Copy)]
enum Operation {
    Binary(ArithKind),
    Increment,
    Negate,
    LessThan,
}

fn program(operation: Operation, feedback: &mut u8) -> crate::CompiledCode {
    let mut ops = Assembler::new().unwrap();
    let entry = ops.offset();
    let refused = ops.new_dynamic_label();
    let mut relocations = RelocationCapture::new(false);
    let mut slow = Vec::new();
    let site = Some(ArithSite {
        cell: feedback as *mut u8 as u64,
        function_id: 7,
        pc: 0,
    });
    dynasm!(ops ; .arch aarch64 ; stp x19, x30, [sp, #-16]! ; mov x19, x0);
    match operation {
        Operation::Binary(kind) => {
            emit_binary_arith(&mut ops, &mut relocations, 2, 0, 1, kind, site, &mut slow).unwrap();
        }
        Operation::Increment => {
            emit_increment(&mut ops, &mut relocations, 2, 0, 1, site, &mut slow).unwrap();
        }
        Operation::Negate => {
            emit_negate(&mut ops, &mut relocations, 2, 0, site, &mut slow).unwrap();
        }
        Operation::LessThan => {
            emit_compare(
                &mut ops,
                &mut relocations,
                2,
                0,
                1,
                CompareKind::Lt,
                site,
                refused,
                refused,
                &mut slow,
            )
            .unwrap();
        }
    }
    emit_load_reg(&mut ops, 0, 2).unwrap();
    dynasm!(ops ; .arch aarch64 ; ldp x19, x30, [sp], #16 ; ret);
    for path in slow {
        let entry = path.entry;
        dynasm!(ops ; .arch aarch64 ; =>entry ; b =>refused);
    }
    dynasm!(ops ; .arch aarch64 ; =>refused
        ; movz x0, VALUE_UNDEFINED as u32
        ; ldp x19, x30, [sp], #16 ; ret);
    crate::CompiledCode::new(&crate::CodeMemory::new(), ops.finalize().unwrap(), entry).unwrap()
}

#[test]
fn template_arithmetic_feedback_uses_numeric_domain_instead_of_boxing() {
    let mixed = ARITH_INT32 | ARITH_FLOAT64;
    for (operation, lhs, rhs, expected, bits) in [
        (
            Operation::Binary(ArithKind::Sub),
            7.0,
            3.0,
            Value::number_i32(4),
            ARITH_INT32,
        ),
        (
            Operation::Binary(ArithKind::Sub),
            -2147483648.0,
            1.0,
            Value::double(-2147483649.0),
            mixed,
        ),
        (
            Operation::Binary(ArithKind::Sub),
            -0.0,
            0.0,
            Value::double(-0.0),
            mixed,
        ),
        (
            Operation::Binary(ArithKind::Sub),
            0.5,
            0.5,
            Value::number_i32(0),
            mixed,
        ),
        (
            Operation::Binary(ArithKind::Mul),
            0.0,
            -1.0,
            Value::double(-0.0),
            mixed,
        ),
        (
            Operation::Binary(ArithKind::Div),
            6.0,
            3.0,
            Value::number_i32(2),
            ARITH_INT32,
        ),
        (
            Operation::Binary(ArithKind::Div),
            7.0,
            2.0,
            Value::double(3.5),
            mixed,
        ),
        (
            Operation::Increment,
            7.0,
            0.0,
            Value::number_i32(8),
            ARITH_INT32,
        ),
        (Operation::Increment, 0.5, 0.0, Value::double(1.5), mixed),
        (
            Operation::Increment,
            2147483647.0,
            0.0,
            Value::double(2147483648.0),
            mixed,
        ),
        (
            Operation::Negate,
            7.0,
            0.0,
            Value::number_i32(-7),
            ARITH_INT32,
        ),
        (Operation::Negate, 0.0, 0.0, Value::double(-0.0), mixed),
        (Operation::Negate, -0.0, 0.0, Value::number_i32(0), mixed),
        (Operation::LessThan, 7.0, 8.0, Value::TRUE, ARITH_INT32),
        (Operation::LessThan, 0.5, 1.0, Value::TRUE, mixed),
        (
            Operation::LessThan,
            f64::NAN,
            f64::NAN,
            Value::FALSE,
            ARITH_FLOAT64,
        ),
    ] {
        for initial in [0, ARITH_FLOAT64] {
            let mut feedback = Box::new(initial);
            let code = program(operation, &mut feedback);
            // SAFETY: the leaf receives one three-word initialized window and
            // preserves the C callee-saved registers it uses. Numeric operands
            // never enter any runtime call or dereference an isolate context.
            let run: extern "C" fn(*mut u64) -> u64 =
                unsafe { std::mem::transmute(code.entry_ptr()) };
            let original = [Value::double(lhs).to_bits(), Value::double(rhs).to_bits()];
            let mut window = [original[0], original[1], Value::undefined().to_bits()];
            let result = Value::from_bits(run(window.as_mut_ptr()));
            assert_eq!(&window[..2], &original, "operand words survive feedback");
            if let Some(number) = expected.as_f64() {
                assert_eq!(result.as_f64().unwrap().to_bits(), number.to_bits());
            } else {
                assert_eq!(result, expected);
            }
            assert_eq!(
                *feedback,
                if initial == 0 { bits } else { initial },
                "fresh observations contribute their domains; FLOAT stays saturated"
            );
        }
    }
}
