use otter_jit::deopt::{
    execute_function_profiled_with_fallback, execute_function_with_fallback, handoff_for_bailout,
};
use otter_jit::pipeline::{JitExecResult, execute_function_profiled_with_runtime};
use otter_vm::FunctionIndex;
use otter_vm::RegisterValue;
use otter_vm::RuntimeState;
use otter_vm::bigint::BigIntTable;
use otter_vm::bytecode::{Bytecode, BytecodeRegister, Instruction, JumpOffset};
use otter_vm::call::{CallSite, CallTable, DirectCall};
use otter_vm::feedback::{FeedbackKind, FeedbackSlotId, FeedbackSlotLayout, FeedbackTableLayout};
use otter_vm::float::FloatTable;
use otter_vm::frame::FrameFlags;
use otter_vm::frame::FrameLayout;
use otter_vm::interpreter::Interpreter;
use otter_vm::module::{Function, FunctionSideTables, FunctionTables, Module};
use otter_vm::object::PropertyValue;
use otter_vm::property::{PropertyNameId, PropertyNameTable};
use otter_vm::regexp::RegExpTable;
use otter_vm::source::compile_script;
use otter_vm::string::StringTable;

fn arithmetic_loop_module(limit: i32) -> otter_vm::module::Module {
    compile_script(
        &format!("var sum = 0; var i = 0; while (i < {limit}) {{ sum += i; i++; }} sum;"),
        "<jit-tier1-loop>",
    )
    .expect("loop script should compile")
}

fn init_entry_registers(function: &Function, runtime: &RuntimeState) -> Vec<RegisterValue> {
    let mut registers =
        vec![RegisterValue::undefined(); usize::from(function.frame_layout().register_count())];
    if let Some(receiver_slot) = function.frame_layout().receiver_slot() {
        let global = runtime.intrinsics().global_object();
        registers[usize::from(receiver_slot)] = RegisterValue::from_object_handle(global.0);
    }
    registers
}

fn read_global_i32(runtime: &mut RuntimeState, name: &str) -> i32 {
    let global = runtime.intrinsics().global_object();
    let property = runtime.intern_property_name(name);
    let lookup = runtime
        .objects()
        .get_property(global, property)
        .expect("global lookup should succeed")
        .expect("global should exist");
    match lookup.value() {
        PropertyValue::Data { value, .. } => value
            .as_i32()
            .expect("global should hold an int32 loop result"),
        PropertyValue::Accessor { .. } => panic!("global should not be an accessor"),
    }
}

fn property_loop_module() -> Module {
    let layout = FrameLayout::new(0, 0, 0, 5).expect("layout should be valid");
    let function = Function::new(
        Some("jit_property_loop"),
        layout,
        Bytecode::from(vec![
            Instruction::new_object(BytecodeRegister::new(0)),
            Instruction::load_i32(BytecodeRegister::new(1), 0),
            Instruction::load_i32(BytecodeRegister::new(2), 1),
            Instruction::load_i32(BytecodeRegister::new(3), 3),
            Instruction::set_property(
                BytecodeRegister::new(0),
                BytecodeRegister::new(1),
                PropertyNameId(0),
            ),
            Instruction::get_property(
                BytecodeRegister::new(1),
                BytecodeRegister::new(0),
                PropertyNameId(0),
            ),
            Instruction::lt(
                BytecodeRegister::new(4),
                BytecodeRegister::new(1),
                BytecodeRegister::new(3),
            ),
            Instruction::jump_if_false(BytecodeRegister::new(4), JumpOffset::new(3)),
            Instruction::add(
                BytecodeRegister::new(1),
                BytecodeRegister::new(1),
                BytecodeRegister::new(2),
            ),
            Instruction::set_property(
                BytecodeRegister::new(0),
                BytecodeRegister::new(1),
                PropertyNameId(0),
            ),
            Instruction::jump(JumpOffset::new(-6)),
            Instruction::ret(BytecodeRegister::new(1)),
        ]),
        FunctionTables::new(
            FunctionSideTables::new(
                PropertyNameTable::new(vec!["count"]),
                StringTable::default(),
                FloatTable::default(),
                BigIntTable::default(),
                otter_vm::closure::ClosureTable::default(),
                CallTable::default(),
                RegExpTable::default(),
            ),
            FeedbackTableLayout::new(vec![
                FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Property),
                FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Property),
                FeedbackSlotLayout::new(FeedbackSlotId(2), FeedbackKind::Property),
                FeedbackSlotLayout::new(FeedbackSlotId(3), FeedbackKind::Property),
                FeedbackSlotLayout::new(FeedbackSlotId(4), FeedbackKind::Property),
                FeedbackSlotLayout::new(FeedbackSlotId(5), FeedbackKind::Property),
                FeedbackSlotLayout::new(FeedbackSlotId(6), FeedbackKind::Branch),
                FeedbackSlotLayout::new(FeedbackSlotId(7), FeedbackKind::Branch),
                FeedbackSlotLayout::new(FeedbackSlotId(8), FeedbackKind::Arithmetic),
                FeedbackSlotLayout::new(FeedbackSlotId(9), FeedbackKind::Property),
                FeedbackSlotLayout::new(FeedbackSlotId(10), FeedbackKind::Branch),
            ]),
            otter_vm::deopt::DeoptTable::default(),
            otter_vm::exception::ExceptionTable::default(),
            otter_vm::source_map::SourceMap::default(),
        ),
    );
    Module::new(Some("jit-property-loop"), vec![function], FunctionIndex(0))
        .expect("module should be valid")
}

fn direct_call_module() -> Module {
    let entry_layout = FrameLayout::new(0, 0, 0, 4).expect("layout should be valid");
    let helper_layout = FrameLayout::new(0, 2, 0, 1).expect("layout should be valid");
    let entry = Function::new(
        Some("jit_direct_call_entry"),
        entry_layout,
        Bytecode::from(vec![
            Instruction::load_i32(BytecodeRegister::new(0), 20),
            Instruction::load_i32(BytecodeRegister::new(1), 22),
            Instruction::call_direct(BytecodeRegister::new(2), BytecodeRegister::new(0)),
            Instruction::ret(BytecodeRegister::new(2)),
        ]),
        FunctionTables::new(
            FunctionSideTables::new(
                PropertyNameTable::default(),
                StringTable::default(),
                FloatTable::default(),
                BigIntTable::default(),
                otter_vm::closure::ClosureTable::default(),
                CallTable::new(vec![
                    None,
                    None,
                    Some(CallSite::Direct(DirectCall::new(
                        FunctionIndex(1),
                        2,
                        FrameFlags::empty(),
                    ))),
                    None,
                ]),
                RegExpTable::default(),
            ),
            FeedbackTableLayout::new(vec![
                FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Call),
                FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Call),
                FeedbackSlotLayout::new(FeedbackSlotId(2), FeedbackKind::Call),
                FeedbackSlotLayout::new(FeedbackSlotId(3), FeedbackKind::Call),
            ]),
            otter_vm::deopt::DeoptTable::default(),
            otter_vm::exception::ExceptionTable::default(),
            otter_vm::source_map::SourceMap::default(),
        ),
    );
    let helper = Function::with_bytecode(
        Some("jit_direct_call_helper"),
        helper_layout,
        Bytecode::from(vec![
            Instruction::add(
                BytecodeRegister::new(2),
                BytecodeRegister::new(0),
                BytecodeRegister::new(1),
            ),
            Instruction::ret(BytecodeRegister::new(2)),
        ]),
    );
    Module::new(
        Some("jit-direct-call"),
        vec![entry, helper],
        FunctionIndex(0),
    )
    .expect("module should be valid")
}

#[test]
fn tier1_loop_smoke_matches_interpreter() {
    let module = arithmetic_loop_module(128);
    let function = module.entry_function();

    let mut interpreter_runtime = RuntimeState::new();
    let interpreter_registers = init_entry_registers(function, &interpreter_runtime);
    let interpreter_result = Interpreter::new()
        .execute_with_runtime(
            &module,
            module.entry(),
            &interpreter_registers,
            &mut interpreter_runtime,
        )
        .expect("interpreter should execute");
    let interpreter_sum = read_global_i32(&mut interpreter_runtime, "sum");

    let mut jit_runtime = RuntimeState::new();
    let mut registers = init_entry_registers(function, &jit_runtime);
    let jit_result = execute_function_profiled_with_runtime(
        &module,
        module.entry(),
        &mut registers,
        &mut jit_runtime,
        &[],
        std::ptr::null(),
    )
    .expect("jit should compile");

    let raw = match jit_result {
        JitExecResult::Ok(raw) => raw,
        JitExecResult::Bailout {
            bytecode_pc,
            reason,
        } => {
            panic!(
                "tier1 loop smoke unexpectedly bailed out at pc {} with {:?}",
                bytecode_pc, reason
            )
        }
        JitExecResult::NotCompiled => panic!("tier1 loop smoke did not compile"),
    };

    let jit_value =
        RegisterValue::from_raw_bits(raw).expect("jit should return a valid vm register value");
    let jit_sum = read_global_i32(&mut jit_runtime, "sum");
    println!(
        "tier1 loop smoke: interpreter={:?} jit={:?} interpreter_sum={} jit_sum={}",
        interpreter_result.return_value(),
        jit_value,
        interpreter_sum,
        jit_sum,
    );
    assert_eq!(jit_value, interpreter_result.return_value());
    assert_eq!(jit_value, RegisterValue::undefined());
    assert_eq!(jit_sum, interpreter_sum);
    assert_eq!(jit_sum, 8128);
}

#[test]
fn unsupported_path_deopts_and_resumes() {
    let layout = FrameLayout::new(0, 0, 0, 5).expect("layout should be valid");
    let function = Function::with_bytecode(
        Some("deopt_resume"),
        layout,
        Bytecode::from(vec![
            Instruction::load_i32(BytecodeRegister::new(0), 40),
            Instruction::load_i32(BytecodeRegister::new(1), 2),
            Instruction::add(
                BytecodeRegister::new(2),
                BytecodeRegister::new(0),
                BytecodeRegister::new(1),
            ),
            Instruction::move_(BytecodeRegister::new(3), BytecodeRegister::new(2)),
            Instruction::new_object(BytecodeRegister::new(4)),
            Instruction::ret(BytecodeRegister::new(3)),
        ]),
    );
    let module = Module::new(Some("deopt-resume"), vec![function], FunctionIndex(0))
        .expect("module should be valid");

    let mut registers = vec![RegisterValue::undefined(); usize::from(layout.register_count())];
    let result =
        execute_function_with_fallback(&module, FunctionIndex(0), &mut registers, std::ptr::null())
            .expect("fallback path should succeed");

    assert_eq!(result.return_value(), RegisterValue::from_i32(42));
}

#[test]
fn safepoint_interrupt_deopts_and_resumes() {
    let module = arithmetic_loop_module(128);
    let function = module.entry_function();
    let interrupt_flag = 1_u8;
    let mut runtime = RuntimeState::new();
    let mut registers = init_entry_registers(function, &runtime);

    let deopt = execute_function_profiled_with_runtime(
        &module,
        module.entry(),
        &mut registers,
        &mut runtime,
        &[],
        std::ptr::addr_of!(interrupt_flag),
    )
    .expect("jit path should execute");

    let (bytecode_pc, reason) = match deopt {
        JitExecResult::Bailout {
            bytecode_pc,
            reason,
        } => (bytecode_pc, reason),
        other => panic!("expected safepoint bailout, got {:?}", other),
    };
    assert_eq!(reason, otter_jit::BailoutReason::Interrupted);
    assert!(bytecode_pc > 0);

    let handoff = handoff_for_bailout(function, bytecode_pc, reason);
    let result = Interpreter::new()
        .resume_with_runtime(
            &module,
            module.entry(),
            handoff.resume_pc(),
            &registers,
            &mut runtime,
        )
        .expect("resume after interrupt should succeed");

    assert_eq!(result.return_value(), RegisterValue::undefined());
    assert_eq!(read_global_i32(&mut runtime, "sum"), 8128);
}

#[test]
fn property_fast_path_smoke() {
    let module = property_loop_module();
    let mut registers = vec![
        RegisterValue::undefined();
        usize::from(module.entry_function().frame_layout().register_count())
    ];

    let result = execute_function_profiled_with_fallback(
        &module,
        module.entry(),
        &mut registers,
        std::ptr::null(),
    )
    .expect("profiled property path should succeed");

    assert_eq!(result.return_value(), RegisterValue::from_i32(3));
}

#[test]
fn direct_call_fast_path_smoke() {
    let module = direct_call_module();
    let mut registers = vec![
        RegisterValue::undefined();
        usize::from(module.entry_function().frame_layout().register_count())
    ];

    let result = execute_function_profiled_with_fallback(
        &module,
        module.entry(),
        &mut registers,
        std::ptr::null(),
    )
    .expect("direct call path should succeed");

    assert_eq!(result.return_value(), RegisterValue::from_i32(42));
}
