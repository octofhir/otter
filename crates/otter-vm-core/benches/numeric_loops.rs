//! Numeric Performance Benchmarks
//!
//! Measures performance of arithmetic operations and quickening (inline caching).

use criterion::{Criterion, criterion_group, criterion_main};
use otter_vm_bytecode::{Function, Instruction, Module, Register};
use otter_vm_core::{GcRef, Interpreter, JsObject, MemoryManager, VmContext, value::Value};
use std::hint::black_box;
use std::sync::Arc;

fn create_test_context() -> VmContext {
    let memory_manager = Arc::new(MemoryManager::test());
    let global = GcRef::new(JsObject::new(Value::null(), memory_manager.clone()));
    VmContext::new(global, memory_manager)
}

/// Benchmark: Simple loop with additions (forces quickening to Int32)
fn bench_int32_loop(c: &mut Criterion) {
    let mut builder = Module::builder("bench.js");
    let iterations = 1000u32;

    let mut func_builder = Function::builder().name("main").feedback_vector_size(1);

    // r0 = counter, r1 = limit, r2 = step
    func_builder = func_builder
        .instruction(Instruction::LoadInt32 {
            dst: Register(0),
            value: 0,
        })
        .instruction(Instruction::LoadInt32 {
            dst: Register(1),
            value: iterations as i32,
        })
        .instruction(Instruction::LoadInt32 {
            dst: Register(2),
            value: 1,
        });

    // Loop: increment counter
    func_builder = func_builder
        .instruction(Instruction::Add {
            dst: Register(0),
            lhs: Register(0),
            rhs: Register(2),
            feedback_index: 0,
        })
        .instruction(Instruction::Lt {
            dst: Register(3),
            lhs: Register(0),
            rhs: Register(1),
        })
        .instruction(Instruction::JumpIfTrue {
            cond: Register(3),
            offset: otter_vm_bytecode::JumpOffset(-3),
        })
        .instruction(Instruction::Return { src: Register(0) });

    let func = func_builder.build();
    builder.add_function(func);
    let module = builder.build();

    c.bench_function("numeric_int32_loop_1000", |b| {
        b.iter(|| {
            let mut ctx = create_test_context();
            let mut interpreter = Interpreter::new();
            let result = interpreter.execute(black_box(&module), &mut ctx).unwrap();
            black_box(result)
        });
    });
}

/// Benchmark: Simple loop with float additions (forces quickening to Number)
fn bench_f64_loop(c: &mut Criterion) {
    let mut builder = Module::builder("bench.js");
    let iterations = 1000u32;
    builder.constants_mut().add_number(0.5);
    builder.constants_mut().add_number(1000.0);

    let mut func_builder = Function::builder().name("main").feedback_vector_size(1);

    // r0 = counter, r1 = limit, r2 = step
    func_builder = func_builder
        .instruction(Instruction::LoadConst {
            dst: Register(0),
            idx: otter_vm_bytecode::ConstantIndex(0),
        })
        .instruction(Instruction::LoadConst {
            dst: Register(1),
            idx: otter_vm_bytecode::ConstantIndex(1),
        })
        .instruction(Instruction::LoadConst {
            dst: Register(2),
            idx: otter_vm_bytecode::ConstantIndex(0),
        });

    // Loop: increment counter
    func_builder = func_builder
        .instruction(Instruction::Add {
            dst: Register(0),
            lhs: Register(0),
            rhs: Register(2),
            feedback_index: 0,
        })
        .instruction(Instruction::Lt {
            dst: Register(3),
            lhs: Register(0),
            rhs: Register(1),
        })
        .instruction(Instruction::JumpIfTrue {
            cond: Register(3),
            offset: otter_vm_bytecode::JumpOffset(-3),
        })
        .instruction(Instruction::Return { src: Register(0) });

    let func = func_builder.build();
    builder.add_function(func);
    let module = builder.build();

    c.bench_function("numeric_f64_loop_1000", |b| {
        b.iter(|| {
            let mut ctx = create_test_context();
            let mut interpreter = Interpreter::new();
            let result = interpreter.execute(black_box(&module), &mut ctx).unwrap();
            black_box(result)
        });
    });
}

criterion_group!(benches, bench_int32_loop, bench_f64_loop);
criterion_main!(benches);
