//! GC-aware property allocation benchmarks.
//!
//! This target is separate from `property_ic`: the IC ratchet keeps a warmed
//! interpreter alive and intentionally avoids explicit collection so it can
//! measure dispatch. The cases here allocate many fresh objects and include an
//! explicit amortized GC cadence in the measured loop, making heap pressure
//! part of the contract instead of an accidental OOM.

use criterion::{Criterion, criterion_group, criterion_main};
use otter_bytecode::{BytecodeModule, Constant, Function, Instruction, Op, Operand, SourceKind};
use otter_vm::{ExecutionContext, Interpreter};
use std::time::Instant;

const GC_CADENCE_RUNS: u32 = 512;

fn instr(pc: u32, op: Op, operands: impl AsRef<[Operand]>) -> Instruction {
    Instruction {
        pc,
        op,
        operands: operands.as_ref().to_vec(),
    }
}

fn string_constant(text: &str) -> Constant {
    Constant::String {
        utf16: text.encode_utf16().collect(),
    }
}

fn new_object_named_store_loop(iterations: i32) -> ExecutionContext {
    let code = vec![
        instr(0, Op::LoadTrue, [Operand::Register(4)]),
        instr(1, Op::LoadInt32, [Operand::Register(1), Operand::Imm32(0)]),
        instr(
            2,
            Op::LoadInt32,
            [Operand::Register(2), Operand::Imm32(iterations)],
        ),
        instr(3, Op::LoadInt32, [Operand::Register(3), Operand::Imm32(1)]),
        instr(
            4,
            Op::LessThan,
            [
                Operand::Register(5),
                Operand::Register(1),
                Operand::Register(2),
            ],
        ),
        instr(
            5,
            Op::JumpIfFalse,
            [Operand::Imm32(4), Operand::Register(5)],
        ),
        instr(6, Op::NewObject, [Operand::Register(0)]),
        instr(
            7,
            Op::StoreProperty,
            [
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::Register(4),
                Operand::Register(8),
            ],
        ),
        instr(
            8,
            Op::Add,
            [
                Operand::Register(1),
                Operand::Register(1),
                Operand::Register(3),
            ],
        ),
        instr(9, Op::Jump, [Operand::Imm32(-6)]),
        instr(10, Op::ReturnUndefined, []),
    ];
    ExecutionContext::from_module(BytecodeModule {
        module: "property-gc-new-object-store-bench.js".to_string(),
        template_sites: Vec::new(),
        source_kind: SourceKind::JavaScript,
        functions: vec![Function {
            id: 0,
            name: "<main>".to_string(),
            scratch: 9,
            code: code.into(),
            ..Function::default()
        }],
        constants: vec![string_constant("foo")],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    })
}

fn new_object_two_named_stores_loop(iterations: i32) -> ExecutionContext {
    let code = vec![
        instr(0, Op::LoadTrue, [Operand::Register(4)]),
        instr(1, Op::LoadFalse, [Operand::Register(6)]),
        instr(2, Op::LoadInt32, [Operand::Register(1), Operand::Imm32(0)]),
        instr(
            3,
            Op::LoadInt32,
            [Operand::Register(2), Operand::Imm32(iterations)],
        ),
        instr(4, Op::LoadInt32, [Operand::Register(3), Operand::Imm32(1)]),
        instr(
            5,
            Op::LessThan,
            [
                Operand::Register(5),
                Operand::Register(1),
                Operand::Register(2),
            ],
        ),
        instr(
            6,
            Op::JumpIfFalse,
            [Operand::Imm32(5), Operand::Register(5)],
        ),
        instr(7, Op::NewObject, [Operand::Register(0)]),
        instr(
            8,
            Op::StoreProperty,
            [
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::Register(4),
                Operand::Register(8),
            ],
        ),
        instr(
            9,
            Op::StoreProperty,
            [
                Operand::Register(0),
                Operand::ConstIndex(1),
                Operand::Register(6),
                Operand::Register(8),
            ],
        ),
        instr(
            10,
            Op::Add,
            [
                Operand::Register(1),
                Operand::Register(1),
                Operand::Register(3),
            ],
        ),
        instr(11, Op::Jump, [Operand::Imm32(-7)]),
        instr(12, Op::ReturnUndefined, []),
    ];
    ExecutionContext::from_module(BytecodeModule {
        module: "property-gc-new-object-two-stores-bench.js".to_string(),
        template_sites: Vec::new(),
        source_kind: SourceKind::JavaScript,
        functions: vec![Function {
            id: 0,
            name: "<main>".to_string(),
            scratch: 9,
            code: code.into(),
            ..Function::default()
        }],
        constants: vec![string_constant("foo"), string_constant("bar")],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    })
}

fn inherited_writable_data_store_loop(iterations: i32) -> ExecutionContext {
    let code = vec![
        instr(0, Op::LoadTrue, [Operand::Register(4)]),
        instr(1, Op::LoadFalse, [Operand::Register(6)]),
        instr(2, Op::NewObject, [Operand::Register(9)]),
        instr(
            3,
            Op::StoreProperty,
            [
                Operand::Register(9),
                Operand::ConstIndex(0),
                Operand::Register(4),
                Operand::Register(8),
            ],
        ),
        instr(4, Op::LoadInt32, [Operand::Register(1), Operand::Imm32(0)]),
        instr(
            5,
            Op::LoadInt32,
            [Operand::Register(2), Operand::Imm32(iterations)],
        ),
        instr(6, Op::LoadInt32, [Operand::Register(3), Operand::Imm32(1)]),
        instr(
            7,
            Op::LessThan,
            [
                Operand::Register(5),
                Operand::Register(1),
                Operand::Register(2),
            ],
        ),
        instr(
            8,
            Op::JumpIfFalse,
            [Operand::Imm32(5), Operand::Register(5)],
        ),
        instr(9, Op::NewObject, [Operand::Register(0)]),
        instr(
            10,
            Op::SetPrototype,
            [Operand::Register(0), Operand::Register(9)],
        ),
        instr(
            11,
            Op::StoreProperty,
            [
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::Register(6),
                Operand::Register(8),
            ],
        ),
        instr(
            12,
            Op::Add,
            [
                Operand::Register(1),
                Operand::Register(1),
                Operand::Register(3),
            ],
        ),
        instr(13, Op::Jump, [Operand::Imm32(-7)]),
        instr(14, Op::ReturnUndefined, []),
    ];
    ExecutionContext::from_module(BytecodeModule {
        module: "property-gc-inherited-data-store-bench.js".to_string(),
        template_sites: Vec::new(),
        source_kind: SourceKind::JavaScript,
        functions: vec![Function {
            id: 0,
            name: "<main>".to_string(),
            scratch: 10,
            code: code.into(),
            ..Function::default()
        }],
        constants: vec![string_constant("foo")],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    })
}

fn inherited_non_writable_data_store_loop(iterations: i32) -> ExecutionContext {
    let code = vec![
        instr(0, Op::LoadTrue, [Operand::Register(4)]),
        instr(1, Op::LoadFalse, [Operand::Register(6)]),
        instr(2, Op::NewObject, [Operand::Register(9)]),
        instr(3, Op::NewObject, [Operand::Register(7)]),
        instr(
            4,
            Op::StoreProperty,
            [
                Operand::Register(7),
                Operand::ConstIndex(1),
                Operand::Register(4),
                Operand::Register(8),
            ],
        ),
        instr(
            5,
            Op::StoreProperty,
            [
                Operand::Register(7),
                Operand::ConstIndex(2),
                Operand::Register(6),
                Operand::Register(8),
            ],
        ),
        instr(
            6,
            Op::LoadString,
            [Operand::Register(10), Operand::ConstIndex(0)],
        ),
        // §10.1.6.1 OrdinaryDefineOwnProperty(target=r9, key=r10, desc=r7).
        instr(
            7,
            Op::DefineOwnProperty,
            [
                Operand::Register(9),
                Operand::Register(10),
                Operand::Register(7),
            ],
        ),
        instr(8, Op::LoadInt32, [Operand::Register(1), Operand::Imm32(0)]),
        instr(
            9,
            Op::LoadInt32,
            [Operand::Register(2), Operand::Imm32(iterations)],
        ),
        instr(10, Op::LoadInt32, [Operand::Register(3), Operand::Imm32(1)]),
        instr(
            11,
            Op::LessThan,
            [
                Operand::Register(5),
                Operand::Register(1),
                Operand::Register(2),
            ],
        ),
        instr(
            12,
            Op::JumpIfFalse,
            [Operand::Imm32(5), Operand::Register(5)],
        ),
        instr(13, Op::NewObject, [Operand::Register(0)]),
        instr(
            14,
            Op::SetPrototype,
            [Operand::Register(0), Operand::Register(9)],
        ),
        instr(
            15,
            Op::StoreProperty,
            [
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::Register(6),
                Operand::Register(8),
            ],
        ),
        instr(
            16,
            Op::Add,
            [
                Operand::Register(1),
                Operand::Register(1),
                Operand::Register(3),
            ],
        ),
        instr(17, Op::Jump, [Operand::Imm32(-7)]),
        instr(18, Op::ReturnUndefined, []),
    ];
    ExecutionContext::from_module(BytecodeModule {
        module: "property-gc-inherited-nonwritable-store-bench.js".to_string(),
        template_sites: Vec::new(),
        source_kind: SourceKind::JavaScript,
        functions: vec![Function {
            id: 0,
            name: "<main>".to_string(),
            scratch: 12,
            code: code.into(),
            ..Function::default()
        }],
        constants: vec![
            string_constant("foo"),
            string_constant("value"),
            string_constant("writable"),
        ],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    })
}

fn direct_prototype_missing_store_loop(iterations: i32) -> ExecutionContext {
    let code = vec![
        instr(0, Op::LoadTrue, [Operand::Register(4)]),
        instr(1, Op::NewObject, [Operand::Register(9)]),
        instr(2, Op::LoadInt32, [Operand::Register(1), Operand::Imm32(0)]),
        instr(
            3,
            Op::LoadInt32,
            [Operand::Register(2), Operand::Imm32(iterations)],
        ),
        instr(4, Op::LoadInt32, [Operand::Register(3), Operand::Imm32(1)]),
        instr(
            5,
            Op::LessThan,
            [
                Operand::Register(5),
                Operand::Register(1),
                Operand::Register(2),
            ],
        ),
        instr(
            6,
            Op::JumpIfFalse,
            [Operand::Imm32(5), Operand::Register(5)],
        ),
        instr(7, Op::NewObject, [Operand::Register(0)]),
        instr(
            8,
            Op::SetPrototype,
            [Operand::Register(0), Operand::Register(9)],
        ),
        instr(
            9,
            Op::StoreProperty,
            [
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::Register(4),
                Operand::Register(8),
            ],
        ),
        instr(
            10,
            Op::Add,
            [
                Operand::Register(1),
                Operand::Register(1),
                Operand::Register(3),
            ],
        ),
        instr(11, Op::Jump, [Operand::Imm32(-7)]),
        instr(12, Op::ReturnUndefined, []),
    ];
    ExecutionContext::from_module(BytecodeModule {
        module: "property-gc-direct-prototype-missing-store-bench.js".to_string(),
        template_sites: Vec::new(),
        source_kind: SourceKind::JavaScript,
        functions: vec![Function {
            id: 0,
            name: "<main>".to_string(),
            scratch: 10,
            code: code.into(),
            ..Function::default()
        }],
        constants: vec![string_constant("foo")],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    })
}

fn primitive_boolean_store_loop(iterations: i32) -> ExecutionContext {
    let code = vec![
        instr(0, Op::LoadFalse, [Operand::Register(0)]),
        instr(1, Op::LoadTrue, [Operand::Register(4)]),
        instr(2, Op::LoadInt32, [Operand::Register(1), Operand::Imm32(0)]),
        instr(
            3,
            Op::LoadInt32,
            [Operand::Register(2), Operand::Imm32(iterations)],
        ),
        instr(4, Op::LoadInt32, [Operand::Register(3), Operand::Imm32(1)]),
        instr(
            5,
            Op::LessThan,
            [
                Operand::Register(5),
                Operand::Register(1),
                Operand::Register(2),
            ],
        ),
        instr(
            6,
            Op::JumpIfFalse,
            [Operand::Imm32(3), Operand::Register(5)],
        ),
        instr(
            7,
            Op::StoreProperty,
            [
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::Register(4),
                Operand::Register(8),
            ],
        ),
        instr(
            8,
            Op::Add,
            [
                Operand::Register(1),
                Operand::Register(1),
                Operand::Register(3),
            ],
        ),
        instr(9, Op::Jump, [Operand::Imm32(-5)]),
        instr(10, Op::ReturnUndefined, []),
    ];
    ExecutionContext::from_module(BytecodeModule {
        module: "property-gc-primitive-boolean-store-bench.js".to_string(),
        template_sites: Vec::new(),
        source_kind: SourceKind::JavaScript,
        functions: vec![Function {
            id: 0,
            name: "<main>".to_string(),
            scratch: 9,
            code: code.into(),
            ..Function::default()
        }],
        constants: vec![string_constant("foo")],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    })
}

fn run_gc_measured_iters(context: &ExecutionContext, iters: u64) -> std::time::Duration {
    let mut interp = Interpreter::new();
    interp.run(context).expect("warm property GC loop");
    interp.force_gc().expect("force GC");
    let mut runs_since_gc = 0_u32;

    let start = Instant::now();
    for _ in 0..iters {
        let value = interp.run(context).expect("property GC bench run");
        std::hint::black_box(value);
        std::hint::black_box(interp.property_ic_stats());
        runs_since_gc += 1;
        if runs_since_gc == GC_CADENCE_RUNS {
            interp.force_gc().expect("force GC");
            runs_since_gc = 0;
            std::hint::black_box(interp.gc_heap_mut().gc_stats().gc_cycles);
        }
    }
    start.elapsed()
}

fn bench_property_gc(c: &mut Criterion) {
    let one_store_context = new_object_named_store_loop(100);
    let two_store_context = new_object_two_named_stores_loop(100);
    let inherited_store_context = inherited_writable_data_store_loop(100);
    let inherited_non_writable_context = inherited_non_writable_data_store_loop(100);
    let direct_proto_missing_context = direct_prototype_missing_store_loop(100);
    let primitive_store_context = primitive_boolean_store_loop(100);

    let mut group = c.benchmark_group("property_gc");
    group.sample_size(10);
    group.warm_up_time(std::time::Duration::from_millis(10));
    group.measurement_time(std::time::Duration::from_millis(200));
    group.bench_function("new_object_named_store_gc512_100", |b| {
        b.iter_custom(|iters| run_gc_measured_iters(&one_store_context, iters));
    });
    group.bench_function("new_object_two_named_stores_gc512_100", |b| {
        b.iter_custom(|iters| run_gc_measured_iters(&two_store_context, iters));
    });
    group.bench_function("inherited_writable_data_store_gc512_100", |b| {
        b.iter_custom(|iters| run_gc_measured_iters(&inherited_store_context, iters));
    });
    group.bench_function("inherited_non_writable_data_store_gc512_100", |b| {
        b.iter_custom(|iters| run_gc_measured_iters(&inherited_non_writable_context, iters));
    });
    group.bench_function("direct_prototype_missing_store_gc512_100", |b| {
        b.iter_custom(|iters| run_gc_measured_iters(&direct_proto_missing_context, iters));
    });
    group.bench_function("primitive_boolean_store_gc512_100", |b| {
        b.iter_custom(|iters| run_gc_measured_iters(&primitive_store_context, iters));
    });
    group.finish();
}

criterion_group!(benches, bench_property_gc);
criterion_main!(benches);
