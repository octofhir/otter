//! Criterion ratchets for named property inline-cache behavior.
//!
//! The benchmark uses hand-built bytecode so it measures the interpreter's
//! property path directly, without source parsing or compiler lowering.
//! The IC is warmed before timing begins; the measured loop repeatedly hits
//! the same `LoadProperty` and `StoreProperty` bytecode sites.
//! A computed string-key loop is measured beside it as the current non-atomized
//! baseline. Own and prototype-chain load-only loops track whether prototype
//! data reads deserve a separate IC slice.

use criterion::{Criterion, criterion_group, criterion_main};
use otter_bytecode::{BytecodeModule, Constant, Function, Instruction, Op, Operand, SourceKind};
use otter_vm::{ExecutionContext, Interpreter};

fn instr(pc: u32, op: Op, operands: impl Into<otter_bytecode::OperandList>) -> Instruction {
    Instruction {
        pc,
        op,
        operands: operands.into(),
    }
}

fn string_constant(text: &str) -> Constant {
    Constant::String {
        utf16: text.encode_utf16().collect(),
    }
}

fn named_property_loop(iterations: i32) -> ExecutionContext {
    let code = vec![
        instr(0, Op::NewObject, [Operand::Register(0)]),
        instr(1, Op::LoadInt32, [Operand::Register(1), Operand::Imm32(0)]),
        instr(
            2,
            Op::LoadInt32,
            [Operand::Register(2), Operand::Imm32(iterations)],
        ),
        instr(3, Op::LoadInt32, [Operand::Register(3), Operand::Imm32(1)]),
        instr(4, Op::LoadTrue, [Operand::Register(4)]),
        instr(
            5,
            Op::StoreProperty,
            [
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::Register(4),
                Operand::Register(8),
            ],
        ),
        instr(
            6,
            Op::LessThan,
            [
                Operand::Register(5),
                Operand::Register(1),
                Operand::Register(2),
            ],
        ),
        instr(
            7,
            Op::JumpIfFalse,
            [Operand::Imm32(4), Operand::Register(5)],
        ),
        instr(
            8,
            Op::LoadProperty,
            [
                Operand::Register(6),
                Operand::Register(0),
                Operand::ConstIndex(0),
            ],
        ),
        instr(
            9,
            Op::StoreProperty,
            [
                Operand::Register(0),
                Operand::ConstIndex(0),
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
        instr(11, Op::Jump, [Operand::Imm32(-6)]),
        instr(12, Op::Return, [Operand::Register(6)]),
    ];
    ExecutionContext::from_module(BytecodeModule {
        module: "property-ic-bench.js".to_string(),
        source_kind: SourceKind::JavaScript,
        functions: vec![Function {
            id: 0,
            name: "<main>".to_string(),
            scratch: 9,
            code,
            ..Function::default()
        }],
        constants: vec![string_constant("foo")],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    })
}

fn computed_string_property_loop(iterations: i32) -> ExecutionContext {
    let code = vec![
        instr(0, Op::NewObject, [Operand::Register(0)]),
        instr(
            1,
            Op::LoadString,
            [Operand::Register(7), Operand::ConstIndex(0)],
        ),
        instr(2, Op::LoadInt32, [Operand::Register(1), Operand::Imm32(0)]),
        instr(
            3,
            Op::LoadInt32,
            [Operand::Register(2), Operand::Imm32(iterations)],
        ),
        instr(4, Op::LoadInt32, [Operand::Register(3), Operand::Imm32(1)]),
        instr(5, Op::LoadTrue, [Operand::Register(4)]),
        instr(
            6,
            Op::StoreElement,
            [
                Operand::Register(0),
                Operand::Register(7),
                Operand::Register(4),
                Operand::Register(8),
            ],
        ),
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
            [Operand::Imm32(4), Operand::Register(5)],
        ),
        instr(
            9,
            Op::LoadElement,
            [
                Operand::Register(6),
                Operand::Register(0),
                Operand::Register(7),
            ],
        ),
        instr(
            10,
            Op::StoreElement,
            [
                Operand::Register(0),
                Operand::Register(7),
                Operand::Register(6),
                Operand::Register(8),
            ],
        ),
        instr(
            11,
            Op::Add,
            [
                Operand::Register(1),
                Operand::Register(1),
                Operand::Register(3),
            ],
        ),
        instr(12, Op::Jump, [Operand::Imm32(-6)]),
        instr(13, Op::Return, [Operand::Register(6)]),
    ];
    ExecutionContext::from_module(BytecodeModule {
        module: "property-computed-bench.js".to_string(),
        source_kind: SourceKind::JavaScript,
        functions: vec![Function {
            id: 0,
            name: "<main>".to_string(),
            scratch: 9,
            code,
            ..Function::default()
        }],
        constants: vec![string_constant("foo")],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    })
}

fn own_named_load_loop(iterations: i32) -> ExecutionContext {
    let code = vec![
        instr(0, Op::NewObject, [Operand::Register(0)]),
        instr(1, Op::LoadTrue, [Operand::Register(4)]),
        instr(
            2,
            Op::StoreProperty,
            [
                Operand::Register(0),
                Operand::ConstIndex(0),
                Operand::Register(4),
                Operand::Register(8),
            ],
        ),
        instr(3, Op::LoadInt32, [Operand::Register(1), Operand::Imm32(0)]),
        instr(
            4,
            Op::LoadInt32,
            [Operand::Register(2), Operand::Imm32(iterations)],
        ),
        instr(5, Op::LoadInt32, [Operand::Register(3), Operand::Imm32(1)]),
        instr(
            6,
            Op::LessThan,
            [
                Operand::Register(5),
                Operand::Register(1),
                Operand::Register(2),
            ],
        ),
        instr(
            7,
            Op::JumpIfFalse,
            [Operand::Imm32(3), Operand::Register(5)],
        ),
        instr(
            8,
            Op::LoadProperty,
            [
                Operand::Register(6),
                Operand::Register(0),
                Operand::ConstIndex(0),
            ],
        ),
        instr(
            9,
            Op::Add,
            [
                Operand::Register(1),
                Operand::Register(1),
                Operand::Register(3),
            ],
        ),
        instr(10, Op::Jump, [Operand::Imm32(-5)]),
        instr(11, Op::Return, [Operand::Register(6)]),
    ];
    ExecutionContext::from_module(BytecodeModule {
        module: "property-own-load-bench.js".to_string(),
        source_kind: SourceKind::JavaScript,
        functions: vec![Function {
            id: 0,
            name: "<main>".to_string(),
            scratch: 9,
            code,
            ..Function::default()
        }],
        constants: vec![string_constant("foo")],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    })
}

fn prototype_named_load_loop(iterations: i32) -> ExecutionContext {
    let code = vec![
        instr(0, Op::NewObject, [Operand::Register(9)]),
        instr(1, Op::LoadTrue, [Operand::Register(4)]),
        instr(
            2,
            Op::StoreProperty,
            [
                Operand::Register(9),
                Operand::ConstIndex(0),
                Operand::Register(4),
                Operand::Register(8),
            ],
        ),
        instr(3, Op::NewObject, [Operand::Register(0)]),
        instr(
            4,
            Op::SetPrototype,
            [Operand::Register(0), Operand::Register(9)],
        ),
        instr(5, Op::LoadInt32, [Operand::Register(1), Operand::Imm32(0)]),
        instr(
            6,
            Op::LoadInt32,
            [Operand::Register(2), Operand::Imm32(iterations)],
        ),
        instr(7, Op::LoadInt32, [Operand::Register(3), Operand::Imm32(1)]),
        instr(
            8,
            Op::LessThan,
            [
                Operand::Register(5),
                Operand::Register(1),
                Operand::Register(2),
            ],
        ),
        instr(
            9,
            Op::JumpIfFalse,
            [Operand::Imm32(3), Operand::Register(5)],
        ),
        instr(
            10,
            Op::LoadProperty,
            [
                Operand::Register(6),
                Operand::Register(0),
                Operand::ConstIndex(0),
            ],
        ),
        instr(
            11,
            Op::Add,
            [
                Operand::Register(1),
                Operand::Register(1),
                Operand::Register(3),
            ],
        ),
        instr(12, Op::Jump, [Operand::Imm32(-5)]),
        instr(13, Op::Return, [Operand::Register(6)]),
    ];
    ExecutionContext::from_module(BytecodeModule {
        module: "property-prototype-load-bench.js".to_string(),
        source_kind: SourceKind::JavaScript,
        functions: vec![Function {
            id: 0,
            name: "<main>".to_string(),
            scratch: 10,
            code,
            ..Function::default()
        }],
        constants: vec![string_constant("foo")],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    })
}

fn bench_property_ic(c: &mut Criterion) {
    let named_context = named_property_loop(1_000);
    let computed_context = computed_string_property_loop(1_000);
    let own_load_context = own_named_load_loop(1_000);
    let prototype_load_context = prototype_named_load_loop(1_000);
    let mut named_interp = Interpreter::new();
    let mut computed_interp = Interpreter::new();
    let mut own_load_interp = Interpreter::new();
    let mut prototype_load_interp = Interpreter::new();
    named_interp.run(&named_context).expect("warm property ICs");
    computed_interp
        .run(&computed_context)
        .expect("warm computed property loop");
    own_load_interp
        .run(&own_load_context)
        .expect("warm own load IC");
    prototype_load_interp
        .run(&prototype_load_context)
        .expect("warm prototype load loop");

    let mut group = c.benchmark_group("property_ic");
    group.sample_size(30);
    group.warm_up_time(std::time::Duration::from_secs(1));
    group.measurement_time(std::time::Duration::from_secs(2));
    group.bench_function("named_load_store_warm_1k", |b| {
        b.iter(|| {
            let value = named_interp
                .run(&named_context)
                .expect("property IC bench run");
            std::hint::black_box(value);
            std::hint::black_box(named_interp.property_ic_stats());
        });
    });
    group.bench_function("computed_string_load_store_1k", |b| {
        b.iter(|| {
            let value = computed_interp
                .run(&computed_context)
                .expect("computed property bench run");
            std::hint::black_box(value);
        });
    });
    group.bench_function("own_named_load_warm_1k", |b| {
        b.iter(|| {
            let value = own_load_interp
                .run(&own_load_context)
                .expect("own named load bench run");
            std::hint::black_box(value);
            std::hint::black_box(own_load_interp.property_ic_stats());
        });
    });
    group.bench_function("prototype_named_load_1k", |b| {
        b.iter(|| {
            let value = prototype_load_interp
                .run(&prototype_load_context)
                .expect("prototype named load bench run");
            std::hint::black_box(value);
            std::hint::black_box(prototype_load_interp.property_ic_stats());
        });
    });
    group.finish();
}

criterion_group!(benches, bench_property_ic);
criterion_main!(benches);
