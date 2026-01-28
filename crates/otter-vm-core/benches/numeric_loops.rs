//! Numeric Loop Performance Benchmarks
//!
//! Measures arithmetic operation performance with type feedback and quickening.

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use otter_vm_bytecode::{Function, Instruction, Module, Register};
use otter_vm_core::{GcRef, Interpreter, JsObject, MemoryManager, VmContext};
use std::sync::Arc;

fn create_test_context() -> VmContext {
    let memory_manager = Arc::new(MemoryManager::test());
    let global = GcRef::new(JsObject::new(None, memory_manager.clone()));
    VmContext::new(global, memory_manager)
}

/// Benchmark: Int32 addition loop (should benefit from quickening)
/// Loops N times adding integers - type feedback should enable fast path
fn bench_int32_addition_loop(c: &mut Criterion) {
    let iterations = 10000u32;

    let mut builder = Module::builder("bench.js");

    // Simple loop: sum = 0; for (i = 0; i < N; i++) sum += 1
    let func = Function::builder()
        .name("main")
        .feedback_vector_size(2)
        // r0 = sum, r1 = counter, r2 = limit, r3 = increment
        .instruction(Instruction::LoadInt32 { dst: Register(0), value: 0 })  // sum = 0
        .instruction(Instruction::LoadInt32 { dst: Register(1), value: 0 })  // i = 0
        .instruction(Instruction::LoadInt32 { dst: Register(2), value: iterations as i32 })  // limit
        .instruction(Instruction::LoadInt32 { dst: Register(3), value: 1 })  // increment
        // Loop start (offset 4):
        .instruction(Instruction::Add {  // sum += 1
            dst: Register(0),
            lhs: Register(0),
            rhs: Register(3),
            feedback_index: 0,
        })
        .instruction(Instruction::Add {  // i += 1
            dst: Register(1),
            lhs: Register(1),
            rhs: Register(3),
            feedback_index: 1,
        })
        .instruction(Instruction::Lt {  // i < limit
            dst: Register(4),
            lhs: Register(1),
            rhs: Register(2),
        })
        .instruction(Instruction::JumpIfTrue {
            cond: Register(4),
            offset: otter_vm_bytecode::JumpOffset(-4),
        })
        .instruction(Instruction::Return { src: Register(0) })
        .build();

    builder.add_function(func);
    let module = builder.build();

    c.bench_function("int32_add_loop_10000", |b| {
        b.iter(|| {
            let mut ctx = create_test_context();
            let mut interpreter = Interpreter::new();
            let result = interpreter.execute(black_box(&module), &mut ctx).unwrap();
            black_box(result)
        });
    });
}

/// Benchmark: Int32 multiplication loop
fn bench_int32_multiplication_loop(c: &mut Criterion) {
    let iterations = 1000u32;

    let mut builder = Module::builder("bench.js");

    // Loop: product = 1; for (i = 0; i < N; i++) product = (product + 1) % 100
    // Avoid overflow by keeping values small
    let func = Function::builder()
        .name("main")
        .feedback_vector_size(3)
        .instruction(Instruction::LoadInt32 { dst: Register(0), value: 1 })  // val = 1
        .instruction(Instruction::LoadInt32 { dst: Register(1), value: 0 })  // i = 0
        .instruction(Instruction::LoadInt32 { dst: Register(2), value: iterations as i32 })  // limit
        .instruction(Instruction::LoadInt32 { dst: Register(3), value: 2 })  // multiplier
        .instruction(Instruction::LoadInt32 { dst: Register(4), value: 1 })  // increment
        .instruction(Instruction::LoadInt32 { dst: Register(5), value: 1000 })  // modulo
        // Loop start (offset 6):
        .instruction(Instruction::Mul {  // val *= 2
            dst: Register(0),
            lhs: Register(0),
            rhs: Register(3),
            feedback_index: 0,
        })
        .instruction(Instruction::Mod {  // val %= 1000 (keep it bounded)
            dst: Register(0),
            lhs: Register(0),
            rhs: Register(5),
        })
        .instruction(Instruction::Add {  // i += 1
            dst: Register(1),
            lhs: Register(1),
            rhs: Register(4),
            feedback_index: 1,
        })
        .instruction(Instruction::Lt {
            dst: Register(6),
            lhs: Register(1),
            rhs: Register(2),
        })
        .instruction(Instruction::JumpIfTrue {
            cond: Register(6),
            offset: otter_vm_bytecode::JumpOffset(-5),
        })
        .instruction(Instruction::Return { src: Register(0) })
        .build();

    builder.add_function(func);
    let module = builder.build();

    c.bench_function("int32_mul_loop_1000", |b| {
        b.iter(|| {
            let mut ctx = create_test_context();
            let mut interpreter = Interpreter::new();
            let result = interpreter.execute(black_box(&module), &mut ctx).unwrap();
            black_box(result)
        });
    });
}

/// Benchmark: Mixed arithmetic operations
fn bench_mixed_arithmetic(c: &mut Criterion) {
    let iterations = 5000u32;

    let mut builder = Module::builder("bench.js");

    let func = Function::builder()
        .name("main")
        .feedback_vector_size(4)
        .instruction(Instruction::LoadInt32 { dst: Register(0), value: 0 })  // sum
        .instruction(Instruction::LoadInt32 { dst: Register(1), value: 0 })  // i
        .instruction(Instruction::LoadInt32 { dst: Register(2), value: iterations as i32 })  // limit
        .instruction(Instruction::LoadInt32 { dst: Register(3), value: 1 })  // 1
        .instruction(Instruction::LoadInt32 { dst: Register(4), value: 2 })  // 2
        // Loop start (offset 5):
        .instruction(Instruction::Add {  // temp = i + 1
            dst: Register(5),
            lhs: Register(1),
            rhs: Register(3),
            feedback_index: 0,
        })
        .instruction(Instruction::Mul {  // temp = temp * 2
            dst: Register(5),
            lhs: Register(5),
            rhs: Register(4),
            feedback_index: 1,
        })
        .instruction(Instruction::Sub {  // temp = temp - 1
            dst: Register(5),
            lhs: Register(5),
            rhs: Register(3),
            feedback_index: 2,
        })
        .instruction(Instruction::Add {  // sum += temp
            dst: Register(0),
            lhs: Register(0),
            rhs: Register(5),
            feedback_index: 3,
        })
        .instruction(Instruction::Add {  // i += 1
            dst: Register(1),
            lhs: Register(1),
            rhs: Register(3),
            feedback_index: 0,  // Reuse slot
        })
        .instruction(Instruction::Lt {
            dst: Register(6),
            lhs: Register(1),
            rhs: Register(2),
        })
        .instruction(Instruction::JumpIfTrue {
            cond: Register(6),
            offset: otter_vm_bytecode::JumpOffset(-7),
        })
        .instruction(Instruction::Return { src: Register(0) })
        .build();

    builder.add_function(func);
    let module = builder.build();

    c.bench_function("mixed_arith_loop_5000", |b| {
        b.iter(|| {
            let mut ctx = create_test_context();
            let mut interpreter = Interpreter::new();
            let result = interpreter.execute(black_box(&module), &mut ctx).unwrap();
            black_box(result)
        });
    });
}

/// Benchmark: Quickened AddI32 opcode directly
fn bench_quickened_add_i32(c: &mut Criterion) {
    let iterations = 10000u32;

    let mut builder = Module::builder("bench.js");

    // Use AddI32 directly (quickened opcode)
    let func = Function::builder()
        .name("main")
        .feedback_vector_size(2)
        .instruction(Instruction::LoadInt32 { dst: Register(0), value: 0 })
        .instruction(Instruction::LoadInt32 { dst: Register(1), value: 0 })
        .instruction(Instruction::LoadInt32 { dst: Register(2), value: iterations as i32 })
        .instruction(Instruction::LoadInt32 { dst: Register(3), value: 1 })
        // Loop using quickened AddI32:
        .instruction(Instruction::AddI32 {
            dst: Register(0),
            lhs: Register(0),
            rhs: Register(3),
            feedback_index: 0,
        })
        .instruction(Instruction::AddI32 {
            dst: Register(1),
            lhs: Register(1),
            rhs: Register(3),
            feedback_index: 1,
        })
        .instruction(Instruction::Lt {
            dst: Register(4),
            lhs: Register(1),
            rhs: Register(2),
        })
        .instruction(Instruction::JumpIfTrue {
            cond: Register(4),
            offset: otter_vm_bytecode::JumpOffset(-4),
        })
        .instruction(Instruction::Return { src: Register(0) })
        .build();

    builder.add_function(func);
    let module = builder.build();

    c.bench_function("quickened_add_i32_loop_10000", |b| {
        b.iter(|| {
            let mut ctx = create_test_context();
            let mut interpreter = Interpreter::new();
            let result = interpreter.execute(black_box(&module), &mut ctx).unwrap();
            black_box(result)
        });
    });
}

criterion_group!(
    benches,
    bench_int32_addition_loop,
    bench_int32_multiplication_loop,
    bench_mixed_arithmetic,
    bench_quickened_add_i32
);
criterion_main!(benches);
