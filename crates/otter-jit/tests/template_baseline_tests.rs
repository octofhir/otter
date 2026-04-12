use otter_jit::baseline::{
    TemplateCompileError, TemplateInstruction, analyze_template_candidate, emit_template_stencil,
};
use otter_jit::code_memory::CompiledCodeOrigin;
use otter_jit::pipeline::{
    JitExecResult, Tier1Strategy, compile_function, execute_function, select_tier1_strategy,
};
use otter_vm::bytecode::{Bytecode, BytecodeRegister, Instruction, JumpOffset, Opcode};
use otter_vm::frame::FrameLayout;
use otter_vm::module::Function;
use otter_vm::RegisterValue;

#[test]
fn template_baseline_recognizes_simple_loop_shape() {
    let function = Function::with_bytecode(
        Some("simple_loop"),
        FrameLayout::new(0, 0, 0, 5).expect("layout"),
        Bytecode::from(vec![
            Instruction::load_i32(BytecodeRegister::new(0), 0),
            Instruction::load_i32(BytecodeRegister::new(1), 0),
            Instruction::load_i32(BytecodeRegister::new(2), 16),
            Instruction::load_i32(BytecodeRegister::new(4), 1),
            Instruction::lt(
                BytecodeRegister::new(3),
                BytecodeRegister::new(1),
                BytecodeRegister::new(2),
            ),
            Instruction::jump_if_false(BytecodeRegister::new(3), JumpOffset::new(3)),
            Instruction::add(
                BytecodeRegister::new(0),
                BytecodeRegister::new(0),
                BytecodeRegister::new(1),
            ),
            Instruction::add(
                BytecodeRegister::new(1),
                BytecodeRegister::new(1),
                BytecodeRegister::new(4),
            ),
            Instruction::jump(JumpOffset::new(-5)),
            Instruction::ret(BytecodeRegister::new(0)),
        ]),
    );

    let program = analyze_template_candidate(&function).expect("loop subset should be recognized");

    assert_eq!(program.function_name, "simple_loop");
    assert_eq!(program.loop_headers, vec![4]);
    assert!(matches!(
        program.instructions.last(),
        Some(TemplateInstruction::Return { src: 0 })
    ));
}

#[test]
fn template_baseline_rejects_property_bytecode_for_now() {
    let function = Function::with_bytecode(
        Some("property"),
        FrameLayout::new(0, 0, 0, 3).expect("layout"),
        Bytecode::from(vec![
            Instruction::new_object(BytecodeRegister::new(0)),
            Instruction::get_property(
                BytecodeRegister::new(1),
                BytecodeRegister::new(0),
                otter_vm::property::PropertyNameId(0),
            ),
            Instruction::ret(BytecodeRegister::new(1)),
        ]),
    );

    let err = analyze_template_candidate(&function).expect_err("property ops are not in slice 1");
    assert_eq!(
        err,
        TemplateCompileError::UnsupportedOpcode {
            pc: 0,
            opcode: Opcode::NewObject,
        }
    );
}

#[test]
fn tier1_strategy_prefers_template_baseline_for_simple_loop() {
    let function = Function::with_bytecode(
        Some("simple_loop"),
        FrameLayout::new(0, 0, 0, 5).expect("layout"),
        Bytecode::from(vec![
            Instruction::load_i32(BytecodeRegister::new(0), 0),
            Instruction::load_i32(BytecodeRegister::new(1), 0),
            Instruction::load_i32(BytecodeRegister::new(2), 16),
            Instruction::load_i32(BytecodeRegister::new(4), 1),
            Instruction::lt(
                BytecodeRegister::new(3),
                BytecodeRegister::new(1),
                BytecodeRegister::new(2),
            ),
            Instruction::jump_if_false(BytecodeRegister::new(3), JumpOffset::new(3)),
            Instruction::add(
                BytecodeRegister::new(0),
                BytecodeRegister::new(0),
                BytecodeRegister::new(1),
            ),
            Instruction::add(
                BytecodeRegister::new(1),
                BytecodeRegister::new(1),
                BytecodeRegister::new(4),
            ),
            Instruction::jump(JumpOffset::new(-5)),
            Instruction::ret(BytecodeRegister::new(0)),
        ]),
    );

    assert_eq!(
        select_tier1_strategy(&function),
        Tier1Strategy::TemplateBaseline
    );
}

#[test]
fn tier1_strategy_falls_back_to_mir_for_property_ops() {
    let function = Function::with_bytecode(
        Some("property"),
        FrameLayout::new(0, 0, 0, 3).expect("layout"),
        Bytecode::from(vec![
            Instruction::new_object(BytecodeRegister::new(0)),
            Instruction::get_property(
                BytecodeRegister::new(1),
                BytecodeRegister::new(0),
                otter_vm::property::PropertyNameId(0),
            ),
            Instruction::ret(BytecodeRegister::new(1)),
        ]),
    );

    assert_eq!(select_tier1_strategy(&function), Tier1Strategy::MirBaseline);
}

#[cfg(target_arch = "aarch64")]
#[test]
fn template_baseline_emits_host_stencil_for_simple_loop() {
    let function = Function::with_bytecode(
        Some("simple_loop"),
        FrameLayout::new(0, 0, 0, 5).expect("layout"),
        Bytecode::from(vec![
            Instruction::load_i32(BytecodeRegister::new(0), 0),
            Instruction::load_i32(BytecodeRegister::new(1), 0),
            Instruction::load_i32(BytecodeRegister::new(2), 16),
            Instruction::load_i32(BytecodeRegister::new(4), 1),
            Instruction::lt(
                BytecodeRegister::new(3),
                BytecodeRegister::new(1),
                BytecodeRegister::new(2),
            ),
            Instruction::jump_if_false(BytecodeRegister::new(3), JumpOffset::new(3)),
            Instruction::add(
                BytecodeRegister::new(0),
                BytecodeRegister::new(0),
                BytecodeRegister::new(1),
            ),
            Instruction::add(
                BytecodeRegister::new(1),
                BytecodeRegister::new(1),
                BytecodeRegister::new(4),
            ),
            Instruction::jump(JumpOffset::new(-5)),
            Instruction::ret(BytecodeRegister::new(0)),
        ]),
    );

    let program = analyze_template_candidate(&function).expect("loop subset should be recognized");
    let buf = emit_template_stencil(&program).expect("host stencil should be emitted");

    assert!(buf.len() >= 16);
    assert_eq!(buf.len() % 4, 0);
    assert_eq!(&buf.bytes()[buf.len() - 4..], &[0xC0, 0x03, 0x5F, 0xD6]);
}

#[cfg(target_arch = "aarch64")]
#[test]
fn template_baseline_compiles_and_executes_as_real_backend() {
    let function = Function::with_bytecode(
        Some("simple_loop"),
        FrameLayout::new(0, 0, 0, 5).expect("layout"),
        Bytecode::from(vec![
            Instruction::load_i32(BytecodeRegister::new(0), 0),
            Instruction::load_i32(BytecodeRegister::new(1), 0),
            Instruction::load_i32(BytecodeRegister::new(2), 16),
            Instruction::load_i32(BytecodeRegister::new(4), 1),
            Instruction::lt(
                BytecodeRegister::new(3),
                BytecodeRegister::new(1),
                BytecodeRegister::new(2),
            ),
            Instruction::jump_if_false(BytecodeRegister::new(3), JumpOffset::new(3)),
            Instruction::add(
                BytecodeRegister::new(0),
                BytecodeRegister::new(0),
                BytecodeRegister::new(1),
            ),
            Instruction::add(
                BytecodeRegister::new(1),
                BytecodeRegister::new(1),
                BytecodeRegister::new(4),
            ),
            Instruction::jump(JumpOffset::new(-5)),
            Instruction::ret(BytecodeRegister::new(0)),
        ]),
    );

    let compiled = compile_function(&function).expect("template baseline should compile");
    assert_eq!(compiled.origin, CompiledCodeOrigin::TemplateBaseline);

    let mut registers =
        vec![RegisterValue::undefined(); usize::from(function.frame_layout().register_count())];
    let result = execute_function(&function, &mut registers).expect("template execution should work");

    match result {
        JitExecResult::Ok(raw) => {
            let value = RegisterValue::from_raw_bits(raw).expect("result should be boxed");
            assert_eq!(value, RegisterValue::from_i32(120));
        }
        other => panic!("template baseline execution unexpectedly failed: {other:?}"),
    }
}
