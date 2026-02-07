//! Inline Cache (IC) Performance Benchmarks
//!
//! Measures property access performance across different IC states.

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;
use otter_vm_bytecode::{ConstantIndex, Function, Instruction, Module, Register};
use otter_vm_core::{GcRef, Interpreter, JsObject, MemoryManager, VmContext, value::Value};
use std::sync::Arc;

fn create_test_context() -> VmContext {
    let memory_manager = Arc::new(MemoryManager::test());
    let global = GcRef::new(JsObject::new(Value::null(), memory_manager.clone()));
    VmContext::new(global, memory_manager)
}

/// Benchmark: Monomorphic property access (IC hits consistently)
/// Accesses the same property on objects with identical shapes.
fn bench_monomorphic_property_access(c: &mut Criterion) {
    // Create bytecode that:
    // 1. Creates an object with property 'x'
    // 2. Reads 'x' in a loop N times
    let mut builder = Module::builder("bench.js");
    builder.constants_mut().add_string("x");

    let iterations = 1000u32;

    let mut func_builder = Function::builder()
        .name("main")
        .feedback_vector_size(2);

    // r0 = new object, r1 = counter, r2 = limit, r3 = temp value
    func_builder = func_builder
        .instruction(Instruction::NewObject { dst: Register(0) })
        .instruction(Instruction::LoadInt32 { dst: Register(1), value: 42 })
        .instruction(Instruction::SetPropConst {
            obj: Register(0),
            name: ConstantIndex(0),
            val: Register(1),
            ic_index: 0,
        })
        .instruction(Instruction::LoadInt32 { dst: Register(1), value: 0 }) // counter
        .instruction(Instruction::LoadInt32 { dst: Register(2), value: iterations as i32 }); // limit

    // Loop: get property and increment counter
    // offset 5: loop start
    func_builder = func_builder
        .instruction(Instruction::GetPropConst {
            dst: Register(3),
            obj: Register(0),
            name: ConstantIndex(0),
            ic_index: 1,
        })
        .instruction(Instruction::LoadInt32 { dst: Register(4), value: 1 })
        .instruction(Instruction::Add {
            dst: Register(1),
            lhs: Register(1),
            rhs: Register(4),
            feedback_index: 0,
        })
        .instruction(Instruction::Lt {
            dst: Register(5),
            lhs: Register(1),
            rhs: Register(2),
        })
        .instruction(Instruction::JumpIfTrue {
            cond: Register(5),
            offset: otter_vm_bytecode::JumpOffset(-5),
        })
        .instruction(Instruction::Return { src: Register(3) });

    let func = func_builder.build();
    builder.add_function(func);
    let module = builder.build();

    c.bench_function("ic_monomorphic_1000_reads", |b| {
        b.iter(|| {
            let mut ctx = create_test_context();
            let mut interpreter = Interpreter::new();
            let result = interpreter.execute(black_box(&module), &mut ctx).unwrap();
            black_box(result)
        });
    });
}

/// Benchmark: Polymorphic property access (IC sees multiple shapes)
/// Accesses the same property on objects with different shapes.
fn bench_polymorphic_property_access(c: &mut Criterion) {
    // Create bytecode that:
    // 1. Creates multiple objects with different shapes but same property 'x'
    // 2. Reads 'x' from each in a loop
    let mut builder = Module::builder("bench.js");
    builder.constants_mut().add_string("x");
    builder.constants_mut().add_string("a");
    builder.constants_mut().add_string("b");

    let mut func_builder = Function::builder()
        .name("main")
        .feedback_vector_size(5);

    // Create 3 objects with different shapes
    // obj1: {x: 1}
    // obj2: {a: 0, x: 2}
    // obj3: {b: 0, x: 3}
    func_builder = func_builder
        // obj1 = {x: 1}
        .instruction(Instruction::NewObject { dst: Register(0) })
        .instruction(Instruction::LoadInt32 { dst: Register(10), value: 1 })
        .instruction(Instruction::SetPropConst {
            obj: Register(0),
            name: ConstantIndex(0), // x
            val: Register(10),
            ic_index: 0,
        })
        // obj2 = {a: 0, x: 2}
        .instruction(Instruction::NewObject { dst: Register(1) })
        .instruction(Instruction::LoadInt32 { dst: Register(10), value: 0 })
        .instruction(Instruction::SetPropConst {
            obj: Register(1),
            name: ConstantIndex(1), // a
            val: Register(10),
            ic_index: 1,
        })
        .instruction(Instruction::LoadInt32 { dst: Register(10), value: 2 })
        .instruction(Instruction::SetPropConst {
            obj: Register(1),
            name: ConstantIndex(0), // x
            val: Register(10),
            ic_index: 2,
        })
        // obj3 = {b: 0, x: 3}
        .instruction(Instruction::NewObject { dst: Register(2) })
        .instruction(Instruction::LoadInt32 { dst: Register(10), value: 0 })
        .instruction(Instruction::SetPropConst {
            obj: Register(2),
            name: ConstantIndex(2), // b
            val: Register(10),
            ic_index: 3,
        })
        .instruction(Instruction::LoadInt32 { dst: Register(10), value: 3 })
        .instruction(Instruction::SetPropConst {
            obj: Register(2),
            name: ConstantIndex(0), // x
            val: Register(10),
            ic_index: 4,
        });

    // Now read x from all three objects 100 times
    // This forces polymorphic IC state
    func_builder = func_builder
        .instruction(Instruction::LoadInt32 { dst: Register(5), value: 0 })  // counter
        .instruction(Instruction::LoadInt32 { dst: Register(6), value: 100 }) // limit
        .instruction(Instruction::LoadInt32 { dst: Register(7), value: 0 });  // accumulator

    // Loop start at offset 18
    func_builder = func_builder
        .instruction(Instruction::GetPropConst {
            dst: Register(8),
            obj: Register(0),
            name: ConstantIndex(0),
            ic_index: 0,
        })
        .instruction(Instruction::Add {
            dst: Register(7),
            lhs: Register(7),
            rhs: Register(8),
            feedback_index: 0,
        })
        .instruction(Instruction::GetPropConst {
            dst: Register(8),
            obj: Register(1),
            name: ConstantIndex(0),
            ic_index: 0, // Same IC slot - polymorphic
        })
        .instruction(Instruction::Add {
            dst: Register(7),
            lhs: Register(7),
            rhs: Register(8),
            feedback_index: 0,
        })
        .instruction(Instruction::GetPropConst {
            dst: Register(8),
            obj: Register(2),
            name: ConstantIndex(0),
            ic_index: 0, // Same IC slot - polymorphic
        })
        .instruction(Instruction::Add {
            dst: Register(7),
            lhs: Register(7),
            rhs: Register(8),
            feedback_index: 0,
        })
        .instruction(Instruction::LoadInt32 { dst: Register(9), value: 1 })
        .instruction(Instruction::Add {
            dst: Register(5),
            lhs: Register(5),
            rhs: Register(9),
            feedback_index: 0,
        })
        .instruction(Instruction::Lt {
            dst: Register(9),
            lhs: Register(5),
            rhs: Register(6),
        })
        .instruction(Instruction::JumpIfTrue {
            cond: Register(9),
            offset: otter_vm_bytecode::JumpOffset(-9),
        })
        .instruction(Instruction::Return { src: Register(7) });

    let func = func_builder.build();
    builder.add_function(func);
    let module = builder.build();

    c.bench_function("ic_polymorphic_300_reads", |b| {
        b.iter(|| {
            let mut ctx = create_test_context();
            let mut interpreter = Interpreter::new();
            let result = interpreter.execute(black_box(&module), &mut ctx).unwrap();
            black_box(result)
        });
    });
}

/// Benchmark: Raw property set performance
fn bench_property_set(c: &mut Criterion) {
    let mut builder = Module::builder("bench.js");
    builder.constants_mut().add_string("x");

    let iterations = 100u32;

    let mut func_builder = Function::builder()
        .name("main")
        .feedback_vector_size(1);

    func_builder = func_builder
        .instruction(Instruction::NewObject { dst: Register(0) })
        .instruction(Instruction::LoadInt32 { dst: Register(1), value: 0 })
        .instruction(Instruction::LoadInt32 { dst: Register(2), value: iterations as i32 });

    // Loop: set property
    func_builder = func_builder
        .instruction(Instruction::SetPropConst {
            obj: Register(0),
            name: ConstantIndex(0),
            val: Register(1),
            ic_index: 0,
        })
        .instruction(Instruction::LoadInt32 { dst: Register(3), value: 1 })
        .instruction(Instruction::Add {
            dst: Register(1),
            lhs: Register(1),
            rhs: Register(3),
            feedback_index: 0,
        })
        .instruction(Instruction::Lt {
            dst: Register(4),
            lhs: Register(1),
            rhs: Register(2),
        })
        .instruction(Instruction::JumpIfTrue {
            cond: Register(4),
            offset: otter_vm_bytecode::JumpOffset(-5),
        })
        .instruction(Instruction::Return { src: Register(1) });

    let func = func_builder.build();
    builder.add_function(func);
    let module = builder.build();

    c.bench_function("ic_property_set_100", |b| {
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
    bench_monomorphic_property_access,
    bench_polymorphic_property_access,
    bench_property_set
);
criterion_main!(benches);
