//! End-to-end codegen tests: bytecode → MIR → CLIF → machine code → execute.

use otter_jit::BAILOUT_SENTINEL;
use otter_jit::code_memory::{compile_clif_function, create_host_isa};
use otter_jit::codegen::lower::lower_mir_to_clif;
use otter_jit::codegen::value_repr;
use otter_jit::context::JitContext;
use otter_jit::mir::graph::{DeoptInfo, MirGraph, ResumeMode};
use otter_jit::mir::nodes::MirOp;
use otter_jit::mir::verify::verify;

/// Build a minimal MIR graph by hand (no bytecode compiler needed).
fn make_return_42_mir() -> MirGraph {
    let mut graph = MirGraph::new("return_42".into(), 0, 1, 0);
    let entry = graph.entry_block;

    // v0 = const.i32 42
    let v0 = graph.push_instr(entry, MirOp::ConstInt32(42), 0);
    // v1 = box_i32 v0
    let v1 = graph.push_instr(entry, MirOp::BoxInt32(v0), 1);
    // return v1
    graph.push_instr(entry, MirOp::Return(v1), 2);

    graph.recompute_edges();
    graph
}

fn make_return_undefined_mir() -> MirGraph {
    let mut graph = MirGraph::new("return_undef".into(), 0, 1, 0);
    let entry = graph.entry_block;
    graph.push_instr(entry, MirOp::ReturnUndefined, 0);
    graph.recompute_edges();
    graph
}

fn make_add_int32_mir() -> MirGraph {
    let mut graph = MirGraph::new("add_int32".into(), 0, 2, 0);
    let entry = graph.entry_block;

    // v0 = const.i32 10
    let v0 = graph.push_instr(entry, MirOp::ConstInt32(10), 0);
    // v1 = const.i32 32
    let v1 = graph.push_instr(entry, MirOp::ConstInt32(32), 1);
    // v2 = add.i32 v0 v1 (with overflow deopt)
    let deopt = graph.create_deopt(DeoptInfo {
        bytecode_pc: 2,
        live_state: vec![],
        resume_mode: ResumeMode::ResumeAtPc,
    });
    let v2 = graph.push_instr(
        entry,
        MirOp::AddI32 {
            lhs: v0,
            rhs: v1,
            deopt,
        },
        2,
    );
    // v3 = box_i32 v2
    let v3 = graph.push_instr(entry, MirOp::BoxInt32(v2), 3);
    // return v3
    graph.push_instr(entry, MirOp::Return(v3), 4);

    graph.recompute_edges();
    graph
}

fn make_add_f64_mir() -> MirGraph {
    let mut graph = MirGraph::new("add_f64".into(), 0, 2, 0);
    let entry = graph.entry_block;

    let v0 = graph.push_instr(entry, MirOp::ConstFloat64(3.14), 0);
    let v1 = graph.push_instr(entry, MirOp::ConstFloat64(2.86), 1);
    let v2 = graph.push_instr(entry, MirOp::AddF64 { lhs: v0, rhs: v1 }, 2);
    let v3 = graph.push_instr(entry, MirOp::BoxFloat64(v2), 3);
    graph.push_instr(entry, MirOp::Return(v3), 4);

    graph.recompute_edges();
    graph
}

/// Execute a MIR graph and return the raw NaN-boxed u64 result.
fn execute_mir(graph: &MirGraph) -> u64 {
    let isa = create_host_isa().expect("failed to create ISA");
    let clif_func = lower_mir_to_clif(graph, isa.as_ref()).expect("lowering failed");
    let compiled = compile_clif_function(clif_func, isa, &[]).expect("compilation failed");

    // Create a minimal JitContext (most fields unused for these simple tests).
    let mut registers = vec![0u64; 16];
    let mut ctx = JitContext {
        registers_base: registers.as_mut_ptr(),
        local_count: 0,
        register_count: 1,
        constants: std::ptr::null(),
        this_raw: value_repr::TAG_UNDEFINED,
        interrupt_flag: std::ptr::null(),
        interpreter: std::ptr::null(),
        vm_ctx: std::ptr::null_mut(),
        function_ptr: std::ptr::null(),
        upvalues_ptr: std::ptr::null(),
        upvalue_count: 0,
        callee_raw: value_repr::TAG_UNDEFINED,
        home_object_raw: value_repr::TAG_UNDEFINED,
        proto_epoch: 0,
        bailout_reason: 0,
        bailout_pc: 0,
        secondary_result: 0,
        module_ptr: std::ptr::null(),
        runtime_ptr: std::ptr::null_mut(),
    };

    unsafe { compiled.call(&mut ctx) }
}

/// Decode a NaN-boxed u64 to check if it's an Int32.
fn decode_int32(bits: u64) -> Option<i32> {
    if (bits & value_repr::INT32_TAG_MASK) == value_repr::TAG_INT32 {
        Some(bits as i32)
    } else {
        None
    }
}

/// Decode a NaN-boxed u64 to check if it's undefined.
fn is_undefined(bits: u64) -> bool {
    bits == value_repr::TAG_UNDEFINED
}

/// Decode a NaN-boxed u64 to f64.
fn decode_f64(bits: u64) -> f64 {
    f64::from_bits(bits)
}

#[test]
fn test_return_42_e2e() {
    let graph = make_return_42_mir();
    assert!(verify(&graph).is_ok());

    let result = execute_mir(&graph);
    assert_ne!(result, BAILOUT_SENTINEL, "should not bail out");
    let value = decode_int32(result);
    assert_eq!(value, Some(42), "expected 42, got bits=0x{:016x}", result);
}

#[test]
fn test_return_undefined_e2e() {
    let graph = make_return_undefined_mir();
    assert!(verify(&graph).is_ok());

    let result = execute_mir(&graph);
    assert!(
        is_undefined(result),
        "expected undefined, got 0x{:016x}",
        result
    );
}

#[test]
fn test_add_int32_e2e() {
    let graph = make_add_int32_mir();
    assert!(verify(&graph).is_ok());

    let result = execute_mir(&graph);
    assert_ne!(result, BAILOUT_SENTINEL, "should not bail out");
    let value = decode_int32(result);
    assert_eq!(value, Some(42), "10 + 32 = 42, got bits=0x{:016x}", result);
}

#[test]
fn test_add_f64_e2e() {
    let graph = make_add_f64_mir();
    assert!(verify(&graph).is_ok());

    let result = execute_mir(&graph);
    assert_ne!(result, BAILOUT_SENTINEL, "should not bail out");
    let value = decode_f64(result);
    assert!(
        (value - 6.0).abs() < 1e-10,
        "3.14 + 2.86 = 6.0, got {}",
        value,
    );
}
