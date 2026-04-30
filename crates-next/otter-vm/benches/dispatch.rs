//! Criterion baseline for the foundation interpreter dispatch loop.
//!
//! Measures the cost of running 10 000 `NOP` instructions followed
//! by a single `RETURN`. The number is the harness slice's pinned
//! baseline — slice tasks compare future dispatch tweaks against
//! this datum.

use criterion::{Criterion, criterion_group, criterion_main};
use otter_bytecode::{BytecodeModule, Function, Instruction, Op, Operand, SourceKind, SpanEntry};
use otter_vm::Interpreter;

fn bench_dispatch(c: &mut Criterion) {
    let mut code = Vec::with_capacity(10_001);
    for pc in 0..10_000 {
        code.push(Instruction {
            pc,
            op: Op::Nop,
            operands: vec![],
        });
    }
    code.push(Instruction {
        pc: 10_000,
        op: Op::Return,
        operands: vec![Operand::Register(0)],
    });
    let spans: Vec<SpanEntry> = code
        .iter()
        .map(|i| SpanEntry {
            pc: i.pc,
            span: (0, 0),
        })
        .collect();
    let module = BytecodeModule {
        module: "bench.ts".to_string(),
        source_kind: SourceKind::TypeScript,
        functions: vec![Function {
            id: 0,
            name: "<main>".to_string(),
            span: (0, 0),
            locals: 0,
            scratch: 1,
            param_count: 0,
            own_upvalue_count: 0,
            is_arrow: false,
            has_rest: false,
            is_async: false,
            is_generator: false,
            is_async_generator: false,
            is_module: false,
            module_url: String::new(),
            code,
            spans,
        }],
        constants: vec![],
        module_resolutions: Vec::new(),
        module_inits: Vec::new(),
    };
    let mut interp = Interpreter::new();
    c.bench_function("dispatch_10k_nop", |b| {
        b.iter(|| interp.run(&module).unwrap());
    });
}

criterion_group!(benches, bench_dispatch);
criterion_main!(benches);
