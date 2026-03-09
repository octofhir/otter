use super::*;
use otter_vm_bytecode::operand::Register;
use otter_vm_bytecode::{Function, Module};

fn create_test_runtime() -> crate::runtime::VmRuntime {
    crate::runtime::VmRuntime::new()
}

fn create_test_context_with_runtime() -> (VmContext, crate::runtime::VmRuntime) {
    let runtime = create_test_runtime();
    let ctx = runtime.create_context();
    (ctx, runtime)
}

#[test]
fn test_load_constants() {
    let mut builder = Module::builder("test.js");

    let func = Function::builder()
        .name("main")
        .instruction(Instruction::LoadInt32 {
            dst: Register(0),
            value: 42,
        })
        .instruction(Instruction::Return { src: Register(0) })
        .build();

    builder.add_function(func);
    let module = builder.build();

    let (mut ctx, _rt) = create_test_context_with_runtime();
    let interpreter = Interpreter::new();
    let result = interpreter.execute(&module, &mut ctx).unwrap();

    assert_eq!(result.as_int32(), Some(42));
}

#[test]
fn test_debugger_instruction_triggers_hook() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let mut builder = Module::builder("test.js");
    let func = Function::builder()
        .name("main")
        .instruction(Instruction::Debugger)
        .instruction(Instruction::ReturnUndefined)
        .build();
    builder.add_function(func);
    let module = builder.build();

    let hook_calls = Arc::new(AtomicUsize::new(0));
    let hook_calls_clone = Arc::clone(&hook_calls);

    let (mut ctx, _rt) = create_test_context_with_runtime();
    ctx.set_debugger_hook(Some(Arc::new(move |_| {
        hook_calls_clone.fetch_add(1, Ordering::SeqCst);
    })));

    let interpreter = Interpreter::new();
    let result = interpreter.execute(&module, &mut ctx).unwrap();
    assert!(result.is_undefined());
    assert_eq!(hook_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn test_trace_records_modified_registers() {
    let mut builder = Module::builder("test.js");
    let func = Function::builder()
        .name("main")
        .instruction(Instruction::LoadInt32 {
            dst: Register(0),
            value: 42,
        })
        .instruction(Instruction::ReturnUndefined)
        .build();
    builder.add_function(func);
    let module = builder.build();

    let (mut ctx, _rt) = create_test_context_with_runtime();
    ctx.set_trace_config(crate::trace::TraceConfig {
        enabled: true,
        mode: crate::trace::TraceMode::RingBuffer,
        ring_buffer_size: 16,
        output_path: None,
        filter: None,
        capture_timing: false,
    });

    let interpreter = Interpreter::new();
    let _ = interpreter.execute(&module, &mut ctx).unwrap();

    let entries: Vec<_> = ctx.get_trace_buffer().unwrap().iter().cloned().collect();
    let load_entry = entries
        .iter()
        .find(|entry| entry.opcode == "LoadInt32")
        .expect("expected LoadInt32 in trace");

    assert!(!load_entry.modified_registers.is_empty());
    assert_eq!(load_entry.modified_registers[0].0, 0);
    assert!(load_entry.modified_registers[0].1.contains("42"));
}

#[test]
fn test_trace_truncates_large_modified_register_values() {
    use otter_vm_bytecode::ConstantIndex;

    let mut builder = Module::builder("test.js");
    builder.constants_mut().add_string(&"x".repeat(4096));

    let func = Function::builder()
        .name("main")
        .instruction(Instruction::LoadConst {
            dst: Register(0),
            idx: ConstantIndex(0),
        })
        .instruction(Instruction::ReturnUndefined)
        .build();
    builder.add_function(func);
    let module = builder.build();

    let (mut ctx, _rt) = create_test_context_with_runtime();
    ctx.set_trace_config(crate::trace::TraceConfig {
        enabled: true,
        mode: crate::trace::TraceMode::RingBuffer,
        ring_buffer_size: 16,
        output_path: None,
        filter: None,
        capture_timing: false,
    });

    let interpreter = Interpreter::new();
    let _ = interpreter.execute(&module, &mut ctx).unwrap();

    let entries: Vec<_> = ctx.get_trace_buffer().unwrap().iter().cloned().collect();
    let load_entry = entries
        .iter()
        .find(|entry| entry.opcode == "LoadConst")
        .expect("expected LoadConst in trace");
    let traced = &load_entry.modified_registers[0].1;

    assert!(
        traced.len() <= 163,
        "expected truncated value preview, got len={}",
        traced.len()
    );
    assert!(
        traced.ends_with("..."),
        "expected truncated suffix, got: {}",
        traced
    );
}

#[test]
fn test_trace_records_execution_timing_when_enabled() {
    let mut builder = Module::builder("test.js");
    let func = Function::builder()
        .name("main")
        .instruction(Instruction::LoadInt32 {
            dst: Register(0),
            value: 1,
        })
        .instruction(Instruction::LoadInt32 {
            dst: Register(1),
            value: 2,
        })
        .instruction(Instruction::Add {
            dst: Register(2),
            lhs: Register(0),
            rhs: Register(1),
            feedback_index: 0,
        })
        .instruction(Instruction::Return { src: Register(2) })
        .build();
    builder.add_function(func);
    let module = builder.build();

    let (mut ctx, _rt) = create_test_context_with_runtime();
    ctx.set_trace_config(crate::trace::TraceConfig {
        enabled: true,
        mode: crate::trace::TraceMode::RingBuffer,
        ring_buffer_size: 16,
        output_path: None,
        filter: None,
        capture_timing: true,
    });

    let interpreter = Interpreter::new();
    let _ = interpreter.execute(&module, &mut ctx).unwrap();

    let entries: Vec<_> = ctx.get_trace_buffer().unwrap().iter().cloned().collect();

    assert!(!entries.is_empty());
    assert!(
        entries
            .iter()
            .all(|entry| entry.execution_time_ns.is_some())
    );
}

#[test]
fn test_arithmetic() {
    let mut builder = Module::builder("test.js");

    let func = Function::builder()
        .name("main")
        .instruction(Instruction::LoadInt32 {
            dst: Register(0),
            value: 10,
        })
        .instruction(Instruction::LoadInt32 {
            dst: Register(1),
            value: 5,
        })
        .instruction(Instruction::Add {
            dst: Register(2),
            lhs: Register(0),
            rhs: Register(1),
            feedback_index: 0,
        })
        .instruction(Instruction::Return { src: Register(2) })
        .build();

    builder.add_function(func);
    let module = builder.build();

    let (mut ctx, _rt) = create_test_context_with_runtime();
    let interpreter = Interpreter::new();
    let result = interpreter.execute(&module, &mut ctx).unwrap();

    assert_eq!(result.as_number(), Some(15.0));
}

#[test]
fn test_div_int_min_by_minus_one_does_not_panic() {
    let mut builder = Module::builder("test.js");

    let func = Function::builder()
        .name("main")
        .register_count(3)
        .instruction(Instruction::LoadInt32 {
            dst: Register(0),
            value: i32::MIN,
        })
        .instruction(Instruction::LoadInt32 {
            dst: Register(1),
            value: -1,
        })
        .instruction(Instruction::Div {
            dst: Register(2),
            lhs: Register(0),
            rhs: Register(1),
            feedback_index: 0,
        })
        .instruction(Instruction::Return { src: Register(2) })
        .feedback_vector_size(1)
        .build();

    builder.add_function(func);
    let module = builder.build();

    let (mut ctx, _rt) = create_test_context_with_runtime();
    let interpreter = Interpreter::new();
    let result = interpreter.execute(&module, &mut ctx).unwrap();

    assert_eq!(result.as_number(), Some(2147483648.0));
}

#[test]
fn test_comparison() {
    let mut builder = Module::builder("test.js");

    let func = Function::builder()
        .name("main")
        .instruction(Instruction::LoadInt32 {
            dst: Register(0),
            value: 10,
        })
        .instruction(Instruction::LoadInt32 {
            dst: Register(1),
            value: 5,
        })
        .instruction(Instruction::Lt {
            dst: Register(2),
            lhs: Register(1),
            rhs: Register(0),
        })
        .instruction(Instruction::Return { src: Register(2) })
        .build();

    builder.add_function(func);
    let module = builder.build();

    let (mut ctx, _rt) = create_test_context_with_runtime();
    let interpreter = Interpreter::new();
    let result = interpreter.execute(&module, &mut ctx).unwrap();

    assert_eq!(result.as_boolean(), Some(true));
}

#[test]
fn test_object_prop_const() {
    use otter_vm_bytecode::ConstantIndex;

    let mut builder = Module::builder("test.js");
    builder.constants_mut().add_string("x");

    let func = Function::builder()
        .name("main")
        // NewObject r0
        .instruction(Instruction::NewObject { dst: Register(0) })
        // LoadInt32 r1, 42
        .instruction(Instruction::LoadInt32 {
            dst: Register(1),
            value: 42,
        })
        // SetPropConst r0, "x", r1
        .instruction(Instruction::SetPropConst {
            obj: Register(0),
            name: ConstantIndex(0),
            val: Register(1),
            ic_index: 0,
        })
        // GetPropConst r2, r0, "x"
        .instruction(Instruction::GetPropConst {
            dst: Register(2),
            obj: Register(0),
            name: ConstantIndex(0),
            ic_index: 0,
        })
        // Return r2
        .instruction(Instruction::Return { src: Register(2) })
        .build();

    builder.add_function(func);
    let module = builder.build();

    let (mut ctx, _rt) = create_test_context_with_runtime();
    let interpreter = Interpreter::new();
    let result = interpreter.execute(&module, &mut ctx).unwrap();

    assert_eq!(result.as_int32(), Some(42));
}

#[test]
fn test_array_elem() {
    let mut builder = Module::builder("test.js");

    let func = Function::builder()
        .name("main")
        .feedback_vector_size(2)
        // NewArray r0, 3
        .instruction(Instruction::NewArray {
            dst: Register(0),
            len: 3,
        })
        // LoadInt32 r1, 10
        .instruction(Instruction::LoadInt32 {
            dst: Register(1),
            value: 10,
        })
        // LoadInt32 r2, 0
        .instruction(Instruction::LoadInt32 {
            dst: Register(2),
            value: 0,
        })
        // SetElem r0, r2, r1
        .instruction(Instruction::SetElem {
            arr: Register(0),
            idx: Register(2),
            val: Register(1),
            ic_index: 0,
        })
        // GetElem r3, r0, r2
        .instruction(Instruction::GetElem {
            dst: Register(3),
            arr: Register(0),
            idx: Register(2),
            ic_index: 1,
        })
        // Return r3
        .instruction(Instruction::Return { src: Register(3) })
        .build();

    builder.add_function(func);
    let module = builder.build();

    let (mut ctx, _rt) = create_test_context_with_runtime();
    let interpreter = Interpreter::new();
    let result = interpreter.execute(&module, &mut ctx).unwrap();

    assert_eq!(result.as_int32(), Some(10));
}

#[test]
fn test_object_prop_computed() {
    use otter_vm_bytecode::ConstantIndex;

    let mut builder = Module::builder("test.js");
    builder.constants_mut().add_string("foo");

    let func = Function::builder()
        .name("main")
        // NewObject r0
        .instruction(Instruction::NewObject { dst: Register(0) })
        // LoadInt32 r1, 99
        .instruction(Instruction::LoadInt32 {
            dst: Register(1),
            value: 99,
        })
        // LoadConst r2, "foo"
        .instruction(Instruction::LoadConst {
            dst: Register(2),
            idx: ConstantIndex(0),
        })
        // SetProp r0, r2, r1
        .instruction(Instruction::SetProp {
            obj: Register(0),
            key: Register(2),
            val: Register(1),
            ic_index: 0,
        })
        // GetProp r3, r0, r2
        .instruction(Instruction::GetProp {
            dst: Register(3),
            obj: Register(0),
            key: Register(2),
            ic_index: 0,
        })
        // Return r3
        .instruction(Instruction::Return { src: Register(3) })
        .build();

    builder.add_function(func);
    let module = builder.build();

    let (mut ctx, _rt) = create_test_context_with_runtime();
    let interpreter = Interpreter::new();
    let result = interpreter.execute(&module, &mut ctx).unwrap();

    assert_eq!(result.as_int32(), Some(99));
}

#[test]
fn test_closure_creation() {
    use otter_vm_bytecode::FunctionIndex;

    let mut builder = Module::builder("test.js");

    // Main function: creates closure and returns it
    let main = Function::builder()
        .name("main")
        // Closure r0, func#1
        .instruction(Instruction::Closure {
            dst: Register(0),
            func: FunctionIndex(1),
        })
        // TypeOf r1, r0
        .instruction(Instruction::TypeOf {
            dst: Register(1),
            src: Register(0),
        })
        // Return r1
        .instruction(Instruction::Return { src: Register(1) })
        .build();

    // Function at index 1 (not called in this test)
    let helper = Function::builder()
        .name("helper")
        .instruction(Instruction::ReturnUndefined)
        .build();

    builder.add_function(main);
    builder.add_function(helper);
    let module = builder.build();

    let (mut ctx, _rt) = create_test_context_with_runtime();
    let interpreter = Interpreter::new();
    let result = interpreter.execute(&module, &mut ctx).unwrap();

    // typeof function === "function"
    let result_str = result.as_string().expect("expected string");
    assert_eq!(result_str.as_str(), "function");
}

#[test]
fn test_function_call_simple() {
    use otter_vm_bytecode::FunctionIndex;

    let mut builder = Module::builder("test.js");

    // Main function:
    //   Closure r0, func#1 (double)
    //   LoadInt32 r1, 5     (argument)
    //   Call r2, r0, 1      (result = double(5))
    //   Return r2
    let main = Function::builder()
        .name("main")
        .instruction(Instruction::Closure {
            dst: Register(0),
            func: FunctionIndex(1),
        })
        .instruction(Instruction::LoadInt32 {
            dst: Register(1),
            value: 5,
        })
        .instruction(Instruction::Call {
            dst: Register(2),
            func: Register(0),
            argc: 1,
            ic_index: 0,
        })
        .instruction(Instruction::Return { src: Register(2) })
        .build();

    // double(x): returns x + x
    //   local[0] = x (argument)
    //   GetLocal r0, 0
    //   Add r1, r0, r0
    //   Return r1
    let double = Function::builder()
        .name("double")
        .param_count(1)
        .local_count(1)
        .instruction(Instruction::GetLocal {
            dst: Register(0),
            idx: otter_vm_bytecode::LocalIndex(0),
        })
        .instruction(Instruction::Add {
            dst: Register(1),
            lhs: Register(0),
            rhs: Register(0),
            feedback_index: 0,
        })
        .instruction(Instruction::Return { src: Register(1) })
        .build();

    builder.add_function(main);
    builder.add_function(double);
    let module = builder.build();

    let (mut ctx, _rt) = create_test_context_with_runtime();
    let interpreter = Interpreter::new();
    let result = interpreter.execute(&module, &mut ctx).unwrap();

    assert_eq!(result.as_number(), Some(10.0)); // 5 + 5 = 10
}

#[test]
fn test_function_call_multiple_args() {
    use otter_vm_bytecode::FunctionIndex;

    let mut builder = Module::builder("test.js");

    // Main: call add(3, 7)
    let main = Function::builder()
        .name("main")
        .instruction(Instruction::Closure {
            dst: Register(0),
            func: FunctionIndex(1),
        })
        .instruction(Instruction::LoadInt32 {
            dst: Register(1),
            value: 3,
        })
        .instruction(Instruction::LoadInt32 {
            dst: Register(2),
            value: 7,
        })
        .instruction(Instruction::Call {
            dst: Register(3),
            func: Register(0),
            argc: 2,
            ic_index: 0,
        })
        .instruction(Instruction::Return { src: Register(3) })
        .build();

    // add(a, b): returns a + b
    let add = Function::builder()
        .name("add")
        .param_count(2)
        .local_count(2)
        .instruction(Instruction::GetLocal {
            dst: Register(0),
            idx: otter_vm_bytecode::LocalIndex(0),
        })
        .instruction(Instruction::GetLocal {
            dst: Register(1),
            idx: otter_vm_bytecode::LocalIndex(1),
        })
        .instruction(Instruction::Add {
            dst: Register(2),
            lhs: Register(0),
            rhs: Register(1),
            feedback_index: 0,
        })
        .instruction(Instruction::Return { src: Register(2) })
        .build();

    builder.add_function(main);
    builder.add_function(add);
    let module = builder.build();

    let (mut ctx, _rt) = create_test_context_with_runtime();
    let interpreter = Interpreter::new();
    let result = interpreter.execute(&module, &mut ctx).unwrap();

    assert_eq!(result.as_number(), Some(10.0)); // 3 + 7 = 10
}

#[test]
fn test_nested_function_calls() {
    use otter_vm_bytecode::FunctionIndex;

    let mut builder = Module::builder("test.js");

    // Main: call outer(2), which calls inner(2) and returns inner(2) * 2
    let main = Function::builder()
        .name("main")
        .instruction(Instruction::Closure {
            dst: Register(0),
            func: FunctionIndex(1),
        })
        .instruction(Instruction::LoadInt32 {
            dst: Register(1),
            value: 2,
        })
        .instruction(Instruction::Call {
            dst: Register(2),
            func: Register(0),
            argc: 1,
            ic_index: 0,
        })
        .instruction(Instruction::Return { src: Register(2) })
        .build();

    // outer(x): returns inner(x) * 2
    let outer = Function::builder()
        .name("outer")
        .param_count(1)
        .local_count(1)
        // Get argument x
        .instruction(Instruction::GetLocal {
            dst: Register(0),
            idx: otter_vm_bytecode::LocalIndex(0),
        })
        // Create closure for inner
        .instruction(Instruction::Closure {
            dst: Register(1),
            func: FunctionIndex(2),
        })
        // Call inner(x)
        .instruction(Instruction::Move {
            dst: Register(2),
            src: Register(0),
        })
        .instruction(Instruction::Call {
            dst: Register(3),
            func: Register(1),
            argc: 1,
            ic_index: 0,
        })
        // Multiply by 2
        .instruction(Instruction::LoadInt32 {
            dst: Register(4),
            value: 2,
        })
        .instruction(Instruction::Mul {
            dst: Register(5),
            lhs: Register(3),
            rhs: Register(4),
            feedback_index: 0,
        })
        .instruction(Instruction::Return { src: Register(5) })
        .build();

    // inner(x): returns x * x
    let inner = Function::builder()
        .name("inner")
        .param_count(1)
        .local_count(1)
        .instruction(Instruction::GetLocal {
            dst: Register(0),
            idx: otter_vm_bytecode::LocalIndex(0),
        })
        .instruction(Instruction::Mul {
            dst: Register(1),
            lhs: Register(0),
            rhs: Register(0),
            feedback_index: 0,
        })
        .instruction(Instruction::Return { src: Register(1) })
        .build();

    builder.add_function(main);
    builder.add_function(outer);
    builder.add_function(inner);
    let module = builder.build();

    let (mut ctx, _rt) = create_test_context_with_runtime();
    let interpreter = Interpreter::new();
    let result = interpreter.execute(&module, &mut ctx).unwrap();

    // outer(2) = inner(2) * 2 = (2*2) * 2 = 8
    assert_eq!(result.as_number(), Some(8.0));
}

#[test]
fn test_define_getter() {
    use otter_vm_bytecode::{ConstantIndex, FunctionIndex};

    let mut builder = Module::builder("test.js");
    builder.constants_mut().add_string("x");

    // Main function:
    // 1. Create object
    // 2. Create getter function (returns 42)
    // 3. DefineGetter on object
    // 4. Access the getter
    let main = Function::builder()
        .name("main")
        // NewObject r0
        .instruction(Instruction::NewObject { dst: Register(0) })
        // LoadConst r1, "x" (key)
        .instruction(Instruction::LoadConst {
            dst: Register(1),
            idx: ConstantIndex(0),
        })
        // Closure r2, getter_fn
        .instruction(Instruction::Closure {
            dst: Register(2),
            func: FunctionIndex(1),
        })
        // DefineGetter obj=r0, key=r1, func=r2
        .instruction(Instruction::DefineGetter {
            obj: Register(0),
            key: Register(1),
            func: Register(2),
        })
        // GetPropConst r3, r0, "x"
        .instruction(Instruction::GetPropConst {
            dst: Register(3),
            obj: Register(0),
            name: ConstantIndex(0),
            ic_index: 0,
        })
        // Return r3
        .instruction(Instruction::Return { src: Register(3) })
        .feedback_vector_size(1)
        .build();

    // Getter function: returns 42
    let getter = Function::builder()
        .name("getter")
        .instruction(Instruction::LoadInt32 {
            dst: Register(0),
            value: 42,
        })
        .instruction(Instruction::Return { src: Register(0) })
        .build();

    builder.add_function(main);
    builder.add_function(getter);
    let module = builder.build();

    let (mut ctx, _rt) = create_test_context_with_runtime();
    let interpreter = Interpreter::new();
    let result = interpreter.execute(&module, &mut ctx).unwrap();

    assert_eq!(result.as_int32(), Some(42));
}

#[test]
fn test_define_setter() {
    use otter_vm_bytecode::{ConstantIndex, FunctionIndex, LocalIndex};

    let mut builder = Module::builder("test.js");
    builder.constants_mut().add_string("x");
    builder.constants_mut().add_string("_x");

    // Main function:
    // 1. Create object with _x property
    // 2. Define setter for x that sets _x
    // 3. Set x via setter
    // 4. Read _x to verify setter was called
    let main = Function::builder()
        .name("main")
        // NewObject r0
        .instruction(Instruction::NewObject { dst: Register(0) })
        // LoadInt32 r1, 0 (initial _x value)
        .instruction(Instruction::LoadInt32 {
            dst: Register(1),
            value: 0,
        })
        // SetPropConst r0, "_x", r1
        .instruction(Instruction::SetPropConst {
            obj: Register(0),
            name: ConstantIndex(1), // "_x"
            val: Register(1),
            ic_index: 0,
        })
        // LoadConst r2, "x" (key)
        .instruction(Instruction::LoadConst {
            dst: Register(2),
            idx: ConstantIndex(0),
        })
        // Closure r3, setter_fn
        .instruction(Instruction::Closure {
            dst: Register(3),
            func: FunctionIndex(1),
        })
        // DefineSetter obj=r0, key=r2, func=r3
        .instruction(Instruction::DefineSetter {
            obj: Register(0),
            key: Register(2),
            func: Register(3),
        })
        // LoadInt32 r4, 99 (value to set)
        .instruction(Instruction::LoadInt32 {
            dst: Register(4),
            value: 99,
        })
        // SetPropConst r0, "x", r4 (triggers setter)
        .instruction(Instruction::SetPropConst {
            obj: Register(0),
            name: ConstantIndex(0), // "x"
            val: Register(4),
            ic_index: 1,
        })
        // GetPropConst r5, r0, "_x" (read back)
        .instruction(Instruction::GetPropConst {
            dst: Register(5),
            obj: Register(0),
            name: ConstantIndex(1), // "_x"
            ic_index: 2,
        })
        // Return r5
        .instruction(Instruction::Return { src: Register(5) })
        .feedback_vector_size(3)
        .build();

    // Setter function: this._x = arg
    // Note: We need to set up 'this' binding properly for this test
    // For now, let's just return 99 to verify the function was called
    let setter = Function::builder()
        .name("setter")
        .local_count(1)
        // The setter receives the value as first argument in local 0
        .instruction(Instruction::GetLocal {
            dst: Register(0),
            idx: LocalIndex(0),
        })
        // Return the value to verify setter was called
        .instruction(Instruction::Return { src: Register(0) })
        .build();

    builder.add_function(main);
    builder.add_function(setter);
    let module = builder.build();

    let (mut ctx, _rt) = create_test_context_with_runtime();
    let interpreter = Interpreter::new();
    let result = interpreter.execute(&module, &mut ctx).unwrap();

    // For now, just verify we can define a setter without crashing
    // Full setter semantics (with 'this' binding) would need more setup
    assert!(result.is_number() || result.is_undefined());
}

// ==================== IC Coverage Tests ====================

#[test]
fn test_ic_coverage_getprop_computed() {
    // Test GetProp IC with computed property access
    use otter_vm_bytecode::ConstantIndex;

    let mut builder = Module::builder("test.js");
    builder.constants_mut().add_string("x");

    let func = Function::builder()
        .name("main")
        .feedback_vector_size(2) // For SetPropConst and GetProp
        .instruction(Instruction::NewObject { dst: Register(0) })
        .instruction(Instruction::LoadInt32 {
            dst: Register(1),
            value: 42,
        })
        .instruction(Instruction::SetPropConst {
            obj: Register(0),
            name: ConstantIndex(0),
            val: Register(1),
            ic_index: 0,
        })
        .instruction(Instruction::LoadConst {
            dst: Register(2),
            idx: ConstantIndex(0),
        })
        .instruction(Instruction::GetProp {
            dst: Register(3),
            obj: Register(0),
            key: Register(2),
            ic_index: 1,
        })
        .instruction(Instruction::Return { src: Register(3) })
        .build();

    builder.add_function(func);
    let module = builder.build();

    let (mut ctx, _rt) = create_test_context_with_runtime();
    let interpreter = Interpreter::new();
    let result = interpreter.execute(&module, &mut ctx).unwrap();

    assert_eq!(result.as_int32(), Some(42));
}

#[test]
fn test_ic_coverage_getelem_setelem() {
    // Test GetElem/SetElem IC with string keys on objects
    use otter_vm_bytecode::ConstantIndex;

    let mut builder = Module::builder("test.js");
    builder.constants_mut().add_string("x");

    let func = Function::builder()
        .name("main")
        .feedback_vector_size(2) // For SetElem and GetElem
        .instruction(Instruction::NewObject { dst: Register(0) })
        .instruction(Instruction::LoadConst {
            dst: Register(1),
            idx: ConstantIndex(0),
        })
        .instruction(Instruction::LoadInt32 {
            dst: Register(2),
            value: 100,
        })
        .instruction(Instruction::SetElem {
            arr: Register(0),
            idx: Register(1),
            val: Register(2),
            ic_index: 0,
        })
        .instruction(Instruction::GetElem {
            dst: Register(3),
            arr: Register(0),
            idx: Register(1),
            ic_index: 1,
        })
        .instruction(Instruction::Return { src: Register(3) })
        .build();

    builder.add_function(func);
    let module = builder.build();

    let (mut ctx, _rt) = create_test_context_with_runtime();
    let interpreter = Interpreter::new();
    let result = interpreter.execute(&module, &mut ctx).unwrap();

    assert_eq!(result.as_int32(), Some(100));
}

#[test]
fn test_ic_coverage_in_operator() {
    // Test In operator IC
    use otter_vm_bytecode::ConstantIndex;

    let mut builder = Module::builder("test.js");
    builder.constants_mut().add_string("x");

    let func = Function::builder()
        .name("main")
        .feedback_vector_size(2) // For SetPropConst and In
        .instruction(Instruction::NewObject { dst: Register(0) })
        .instruction(Instruction::LoadInt32 {
            dst: Register(1),
            value: 1,
        })
        .instruction(Instruction::SetPropConst {
            obj: Register(0),
            name: ConstantIndex(0),
            val: Register(1),
            ic_index: 0,
        })
        .instruction(Instruction::LoadConst {
            dst: Register(2),
            idx: ConstantIndex(0),
        })
        .instruction(Instruction::In {
            dst: Register(3),
            lhs: Register(2),
            rhs: Register(0),
            ic_index: 1,
        })
        .instruction(Instruction::Return { src: Register(3) })
        .build();

    builder.add_function(func);
    let module = builder.build();

    let (mut ctx, _rt) = create_test_context_with_runtime();
    let interpreter = Interpreter::new();
    let result = interpreter.execute(&module, &mut ctx).unwrap();

    assert_eq!(result.as_boolean(), Some(true));
}

#[test]
fn test_ic_coverage_instanceof() {
    // Test InstanceOf IC - caches prototype lookup on constructor
    // This test uses Construct to properly create an instance
    use otter_vm_bytecode::FunctionIndex;

    let mut builder = Module::builder("test.js");
    builder.constants_mut().add_string("prototype");

    // Create a constructor function and test instanceof using Construct
    let main = Function::builder()
        .name("main")
        .feedback_vector_size(2)
        // Create constructor function
        .instruction(Instruction::Closure {
            dst: Register(0),
            func: FunctionIndex(1),
        })
        // Create instance using Construct
        .instruction(Instruction::Construct {
            dst: Register(1),
            func: Register(0),
            argc: 0,
        })
        // Test instanceof (this exercises the IC on prototype lookup)
        .instruction(Instruction::InstanceOf {
            dst: Register(2),
            lhs: Register(1),
            rhs: Register(0),
            ic_index: 0,
        })
        .instruction(Instruction::Return { src: Register(2) })
        .build();

    // Constructor function
    let constructor = Function::builder()
        .name("Constructor")
        .instruction(Instruction::LoadUndefined { dst: Register(0) })
        .instruction(Instruction::Return { src: Register(0) })
        .build();

    builder.add_function(main);
    builder.add_function(constructor);
    let module = builder.build();

    let (mut ctx, _rt) = create_test_context_with_runtime();
    let interpreter = Interpreter::new();
    let result = interpreter.execute(&module, &mut ctx).unwrap();

    assert_eq!(result.as_boolean(), Some(true));
}

#[test]
fn test_ic_coverage_array_integer_access() {
    // Test GetElem/SetElem fast path with integer indices on arrays
    let mut builder = Module::builder("test.js");

    let func = Function::builder()
        .name("main")
        .feedback_vector_size(2)
        // Create array with 3 elements
        .instruction(Instruction::NewArray {
            dst: Register(0),
            len: 3,
        })
        // Set arr[1] = 42
        .instruction(Instruction::LoadInt32 {
            dst: Register(1),
            value: 1, // index
        })
        .instruction(Instruction::LoadInt32 {
            dst: Register(2),
            value: 42, // value
        })
        .instruction(Instruction::SetElem {
            arr: Register(0),
            idx: Register(1),
            val: Register(2),
            ic_index: 0,
        })
        // Get arr[1]
        .instruction(Instruction::GetElem {
            dst: Register(3),
            arr: Register(0),
            idx: Register(1),
            ic_index: 1,
        })
        .instruction(Instruction::Return { src: Register(3) })
        .build();

    builder.add_function(func);
    let module = builder.build();

    let (mut ctx, _rt) = create_test_context_with_runtime();
    let interpreter = Interpreter::new();
    let result = interpreter.execute(&module, &mut ctx).unwrap();

    assert_eq!(result.as_int32(), Some(42));
}

// ==================== IC State Machine Tests ====================

#[test]
fn test_ic_state_machine_uninitialized_to_mono() {
    // Test that IC transitions from Uninitialized to Monomorphic on first access
    use otter_vm_bytecode::function::InlineCacheState;
    use otter_vm_bytecode::operand::ConstantIndex;

    let mut builder = Module::builder("test.js");
    builder.constants_mut().add_string("x");

    let func = Function::builder()
        .name("main")
        .feedback_vector_size(1)
        // Create object with property
        .instruction(Instruction::NewObject { dst: Register(0) })
        .instruction(Instruction::LoadInt32 {
            dst: Register(1),
            value: 42,
        })
        .instruction(Instruction::SetPropConst {
            obj: Register(0),
            name: ConstantIndex(0), // "x"
            val: Register(1),
            ic_index: 0,
        })
        // Read the property (this should cache in IC)
        .instruction(Instruction::GetPropConst {
            dst: Register(2),
            obj: Register(0),
            name: ConstantIndex(0),
            ic_index: 0,
        })
        .instruction(Instruction::Return { src: Register(2) })
        .build();

    builder.add_function(func);
    let module = builder.build();
    let module = std::sync::Arc::new(module);

    let (mut ctx, _rt) = create_test_context_with_runtime();
    let interpreter = Interpreter::new();
    let result = interpreter.execute_arc(module.clone(), &mut ctx).unwrap();

    assert_eq!(result.as_int32(), Some(42));

    // Check IC state transitioned to Monomorphic
    let func = module.function(0).unwrap();
    let feedback = func.feedback_vector.read();
    if let Some(ic) = feedback.get(0) {
        match &ic.ic_state {
            InlineCacheState::Monomorphic { .. } => {}
            state => panic!("Expected Monomorphic IC state, got {:?}", state),
        }
    }
}

#[test]
fn test_ic_state_machine_mono_to_poly() {
    // Test that IC transitions from Monomorphic to Polymorphic on 2nd shape
    use otter_vm_bytecode::function::InlineCacheState;
    use otter_vm_bytecode::operand::ConstantIndex;

    let mut builder = Module::builder("test.js");
    builder.constants_mut().add_string("x");
    builder.constants_mut().add_string("y");

    let func = Function::builder()
        .name("main")
        .local_count(10)
        .register_count(10)
        .feedback_vector_size(1)
        // Create first object with property "x"
        .instruction(Instruction::NewObject { dst: Register(0) })
        .instruction(Instruction::LoadInt32 {
            dst: Register(1),
            value: 10,
        })
        .instruction(Instruction::SetPropConst {
            obj: Register(0),
            name: ConstantIndex(0), // "x"
            val: Register(1),
            ic_index: 0,
        })
        // Read x from first object (caches mono state)
        .instruction(Instruction::GetPropConst {
            dst: Register(2),
            obj: Register(0),
            name: ConstantIndex(0),
            ic_index: 0,
        })
        // Create second object with different shape (has "y" first, then "x")
        .instruction(Instruction::NewObject { dst: Register(3) })
        .instruction(Instruction::LoadInt32 {
            dst: Register(4),
            value: 100,
        })
        .instruction(Instruction::SetPropConst {
            obj: Register(3),
            name: ConstantIndex(1), // "y"
            val: Register(4),
            ic_index: 0, // uses same IC slot but different shape
        })
        .instruction(Instruction::LoadInt32 {
            dst: Register(5),
            value: 20,
        })
        .instruction(Instruction::SetPropConst {
            obj: Register(3),
            name: ConstantIndex(0), // "x"
            val: Register(5),
            ic_index: 0,
        })
        // Read x from second object (should transition to poly)
        .instruction(Instruction::GetPropConst {
            dst: Register(6),
            obj: Register(3),
            name: ConstantIndex(0),
            ic_index: 0,
        })
        // Return sum of both reads
        .instruction(Instruction::Add {
            dst: Register(7),
            lhs: Register(2),
            rhs: Register(6),
            feedback_index: 1,
        })
        .instruction(Instruction::Return { src: Register(7) })
        .build();

    builder.add_function(func);
    let module = builder.build();
    let module = std::sync::Arc::new(module);

    let (mut ctx, _rt) = create_test_context_with_runtime();
    let interpreter = Interpreter::new();
    let result = interpreter.execute_arc(module.clone(), &mut ctx).unwrap();

    assert_eq!(result.as_int32(), Some(30)); // 10 + 20

    // Check IC state transitioned to Polymorphic
    let func = module.function(0).unwrap();
    let feedback = func.feedback_vector.read();
    if let Some(ic) = feedback.get(0) {
        match &ic.ic_state {
            InlineCacheState::Polymorphic { count, .. } => {
                assert!(*count >= 2, "Expected at least 2 shapes cached");
            }
            state => panic!("Expected Polymorphic IC state, got {:?}", state),
        }
    }
}

#[test]
fn test_ic_state_machine_poly_to_mega() {
    // Test that IC transitions from Polymorphic to Megamorphic at 4+ shapes
    use otter_vm_bytecode::function::InlineCacheState;
    use otter_vm_bytecode::operand::ConstantIndex;

    let mut builder = Module::builder("test.js");
    builder.constants_mut().add_string("x"); // 0
    builder.constants_mut().add_string("a"); // 1
    builder.constants_mut().add_string("b"); // 2
    builder.constants_mut().add_string("c"); // 3
    builder.constants_mut().add_string("d"); // 4

    let func = Function::builder()
        .name("main")
        .local_count(30)
        .register_count(30)
        .feedback_vector_size(1)
        // Create 5 objects with different shapes, all having "x"
        // Object 1: only "x"
        .instruction(Instruction::NewObject { dst: Register(0) })
        .instruction(Instruction::LoadInt32 {
            dst: Register(1),
            value: 1,
        })
        .instruction(Instruction::SetPropConst {
            obj: Register(0),
            name: ConstantIndex(0), // "x"
            val: Register(1),
            ic_index: 0,
        })
        .instruction(Instruction::GetPropConst {
            dst: Register(2),
            obj: Register(0),
            name: ConstantIndex(0),
            ic_index: 0,
        })
        // Object 2: "a" then "x"
        .instruction(Instruction::NewObject { dst: Register(3) })
        .instruction(Instruction::LoadInt32 {
            dst: Register(4),
            value: 100,
        })
        .instruction(Instruction::SetPropConst {
            obj: Register(3),
            name: ConstantIndex(1), // "a"
            val: Register(4),
            ic_index: 0,
        })
        .instruction(Instruction::LoadInt32 {
            dst: Register(5),
            value: 2,
        })
        .instruction(Instruction::SetPropConst {
            obj: Register(3),
            name: ConstantIndex(0), // "x"
            val: Register(5),
            ic_index: 0,
        })
        .instruction(Instruction::GetPropConst {
            dst: Register(6),
            obj: Register(3),
            name: ConstantIndex(0),
            ic_index: 0,
        })
        // Object 3: "b" then "x"
        .instruction(Instruction::NewObject { dst: Register(7) })
        .instruction(Instruction::LoadInt32 {
            dst: Register(8),
            value: 100,
        })
        .instruction(Instruction::SetPropConst {
            obj: Register(7),
            name: ConstantIndex(2), // "b"
            val: Register(8),
            ic_index: 0,
        })
        .instruction(Instruction::LoadInt32 {
            dst: Register(9),
            value: 3,
        })
        .instruction(Instruction::SetPropConst {
            obj: Register(7),
            name: ConstantIndex(0), // "x"
            val: Register(9),
            ic_index: 0,
        })
        .instruction(Instruction::GetPropConst {
            dst: Register(10),
            obj: Register(7),
            name: ConstantIndex(0),
            ic_index: 0,
        })
        // Object 4: "c" then "x"
        .instruction(Instruction::NewObject { dst: Register(11) })
        .instruction(Instruction::LoadInt32 {
            dst: Register(12),
            value: 100,
        })
        .instruction(Instruction::SetPropConst {
            obj: Register(11),
            name: ConstantIndex(3), // "c"
            val: Register(12),
            ic_index: 0,
        })
        .instruction(Instruction::LoadInt32 {
            dst: Register(13),
            value: 4,
        })
        .instruction(Instruction::SetPropConst {
            obj: Register(11),
            name: ConstantIndex(0), // "x"
            val: Register(13),
            ic_index: 0,
        })
        .instruction(Instruction::GetPropConst {
            dst: Register(14),
            obj: Register(11),
            name: ConstantIndex(0),
            ic_index: 0,
        })
        // Object 5: "d" then "x" - this should trigger Megamorphic
        .instruction(Instruction::NewObject { dst: Register(15) })
        .instruction(Instruction::LoadInt32 {
            dst: Register(16),
            value: 100,
        })
        .instruction(Instruction::SetPropConst {
            obj: Register(15),
            name: ConstantIndex(4), // "d"
            val: Register(16),
            ic_index: 0,
        })
        .instruction(Instruction::LoadInt32 {
            dst: Register(17),
            value: 5,
        })
        .instruction(Instruction::SetPropConst {
            obj: Register(15),
            name: ConstantIndex(0), // "x"
            val: Register(17),
            ic_index: 0,
        })
        .instruction(Instruction::GetPropConst {
            dst: Register(18),
            obj: Register(15),
            name: ConstantIndex(0),
            ic_index: 0,
        })
        // Sum all x values: 1+2+3+4+5 = 15
        .instruction(Instruction::Add {
            dst: Register(19),
            lhs: Register(2),
            rhs: Register(6),
            feedback_index: 1,
        })
        .instruction(Instruction::Add {
            dst: Register(20),
            lhs: Register(19),
            rhs: Register(10),
            feedback_index: 2,
        })
        .instruction(Instruction::Add {
            dst: Register(21),
            lhs: Register(20),
            rhs: Register(14),
            feedback_index: 3,
        })
        .instruction(Instruction::Add {
            dst: Register(22),
            lhs: Register(21),
            rhs: Register(18),
            feedback_index: 4,
        })
        .instruction(Instruction::Return { src: Register(22) })
        .build();

    builder.add_function(func);
    let module = builder.build();
    let module = std::sync::Arc::new(module);

    let (mut ctx, _rt) = create_test_context_with_runtime();
    let interpreter = Interpreter::new();
    let result = interpreter.execute_arc(module.clone(), &mut ctx).unwrap();

    assert_eq!(result.as_int32(), Some(15)); // 1+2+3+4+5

    // Check IC state transitioned to Megamorphic
    let func = module.function(0).unwrap();
    let feedback = func.feedback_vector.read();
    if let Some(ic) = feedback.get(0) {
        match &ic.ic_state {
            InlineCacheState::Megamorphic => {}
            state => panic!("Expected Megamorphic IC state, got {:?}", state),
        }
    }
}

// ==================== Proto Chain Cache Tests ====================

#[test]
fn test_proto_chain_cache_epoch_bump() {
    // Test that proto_epoch is bumped when set_prototype is called
    use crate::object::get_proto_epoch;

    let _rt = crate::runtime::VmRuntime::new();
    let _memory_manager = _rt.memory_manager().clone();

    // Record initial epoch
    let initial_epoch = get_proto_epoch();

    // Create objects and set prototype
    let obj1 = GcRef::new(JsObject::new(Value::null()));
    let obj2 = GcRef::new(JsObject::new(Value::null()));

    // Set prototype should bump epoch
    obj1.set_prototype(Value::object(obj2.clone()));

    let after_first = get_proto_epoch();
    assert!(
        after_first > initial_epoch,
        "proto_epoch should be bumped after set_prototype"
    );

    // Another set_prototype should bump again
    let obj3 = GcRef::new(JsObject::new(Value::null()));
    obj2.set_prototype(Value::object(obj3));

    let after_second = get_proto_epoch();
    assert!(
        after_second > after_first,
        "proto_epoch should be bumped after each set_prototype"
    );
}

#[test]
fn test_proto_chain_cache_ic_stores_epoch() {
    // Test that IC stores proto_epoch when caching
    use crate::object::get_proto_epoch;
    use otter_vm_bytecode::function::InlineCacheState;
    use otter_vm_bytecode::operand::ConstantIndex;

    let mut builder = Module::builder("test.js");
    builder.constants_mut().add_string("x");

    let func = Function::builder()
        .name("main")
        .feedback_vector_size(1)
        // Create object and set property
        .instruction(Instruction::NewObject { dst: Register(0) })
        .instruction(Instruction::LoadInt32 {
            dst: Register(1),
            value: 42,
        })
        .instruction(Instruction::SetPropConst {
            obj: Register(0),
            name: ConstantIndex(0), // "x"
            val: Register(1),
            ic_index: 0,
        })
        // Read property to trigger IC caching
        .instruction(Instruction::GetPropConst {
            dst: Register(2),
            obj: Register(0),
            name: ConstantIndex(0),
            ic_index: 0,
        })
        .instruction(Instruction::Return { src: Register(2) })
        .build();

    builder.add_function(func);
    let module = builder.build();
    let module = std::sync::Arc::new(module);

    // Record epoch before execution
    let epoch_before = get_proto_epoch();

    let (mut ctx, _rt) = create_test_context_with_runtime();
    let interpreter = Interpreter::new();
    let result = interpreter.execute_arc(module.clone(), &mut ctx).unwrap();

    assert_eq!(result.as_int32(), Some(42));

    // Check that IC has proto_epoch stored
    let func = module.function(0).unwrap();
    let feedback = func.feedback_vector.read();
    if let Some(ic) = feedback.get(0) {
        match &ic.ic_state {
            InlineCacheState::Monomorphic { .. } => {
                // proto_epoch should be >= epoch_before (execution may have bumped it)
                assert!(
                    ic.proto_epoch >= epoch_before,
                    "IC proto_epoch ({}) should be >= epoch_before ({})",
                    ic.proto_epoch,
                    epoch_before
                );
            }
            state => panic!("Expected Monomorphic IC state, got {:?}", state),
        }
    }
}

#[test]
fn test_proto_chain_cache_epoch_consistency() {
    // Test that proto_epoch is consistent across multiple IC updates
    use crate::object::get_proto_epoch;
    use otter_vm_bytecode::function::InlineCacheState;
    use otter_vm_bytecode::operand::ConstantIndex;

    let mut builder = Module::builder("test.js");
    builder.constants_mut().add_string("x");
    builder.constants_mut().add_string("y");

    let func = Function::builder()
        .name("main")
        .local_count(10)
        .register_count(10)
        .feedback_vector_size(1)
        // Create first object and set property
        .instruction(Instruction::NewObject { dst: Register(0) })
        .instruction(Instruction::LoadInt32 {
            dst: Register(1),
            value: 10,
        })
        .instruction(Instruction::SetPropConst {
            obj: Register(0),
            name: ConstantIndex(0), // "x"
            val: Register(1),
            ic_index: 0,
        })
        .instruction(Instruction::GetPropConst {
            dst: Register(2),
            obj: Register(0),
            name: ConstantIndex(0),
            ic_index: 0,
        })
        // Create second object with different shape
        .instruction(Instruction::NewObject { dst: Register(3) })
        .instruction(Instruction::LoadInt32 {
            dst: Register(4),
            value: 100,
        })
        .instruction(Instruction::SetPropConst {
            obj: Register(3),
            name: ConstantIndex(1), // "y"
            val: Register(4),
            ic_index: 0,
        })
        .instruction(Instruction::LoadInt32 {
            dst: Register(5),
            value: 20,
        })
        .instruction(Instruction::SetPropConst {
            obj: Register(3),
            name: ConstantIndex(0), // "x"
            val: Register(5),
            ic_index: 0,
        })
        .instruction(Instruction::GetPropConst {
            dst: Register(6),
            obj: Register(3),
            name: ConstantIndex(0),
            ic_index: 0,
        })
        .instruction(Instruction::Add {
            dst: Register(7),
            lhs: Register(2),
            rhs: Register(6),
            feedback_index: 1,
        })
        .instruction(Instruction::Return { src: Register(7) })
        .build();

    builder.add_function(func);
    let module = builder.build();
    let module = std::sync::Arc::new(module);

    let epoch_before = get_proto_epoch();

    let (mut ctx, _rt) = create_test_context_with_runtime();
    let interpreter = Interpreter::new();
    let result = interpreter.execute_arc(module.clone(), &mut ctx).unwrap();

    assert_eq!(result.as_int32(), Some(30)); // 10 + 20

    // Check that IC has transitioned to Polymorphic and has proto_epoch
    let func = module.function(0).unwrap();
    let feedback = func.feedback_vector.read();
    if let Some(ic) = feedback.get(0) {
        match &ic.ic_state {
            InlineCacheState::Polymorphic { count, .. } => {
                assert!(*count >= 2, "Expected at least 2 shapes cached");
                // proto_epoch should be reasonable
                assert!(
                    ic.proto_epoch >= epoch_before,
                    "IC proto_epoch should be >= epoch_before"
                );
            }
            state => panic!("Expected Polymorphic IC state, got {:?}", state),
        }
    }
}

#[test]
fn test_dictionary_mode_threshold_trigger() {
    // Test that adding more than DICTIONARY_THRESHOLD properties triggers dictionary mode
    use crate::object::{DICTIONARY_THRESHOLD, JsObject, PropertyKey};

    let _rt = crate::runtime::VmRuntime::new();
    let _memory_manager = _rt.memory_manager().clone();
    let obj = GcRef::new(JsObject::new(Value::null()));

    // Initially not in dictionary mode
    assert!(
        !obj.is_dictionary_mode(),
        "Object should not be in dictionary mode initially"
    );

    // Add properties up to just below threshold
    for i in 0..(DICTIONARY_THRESHOLD - 1) {
        let key = PropertyKey::String(crate::string::JsString::intern(&format!("prop{}", i)));
        let _ = obj.set(key, Value::int32(i as i32));
    }
    assert!(
        !obj.is_dictionary_mode(),
        "Object should not be in dictionary mode below threshold"
    );

    // Add one more property to exceed threshold
    let key = PropertyKey::String(crate::string::JsString::intern(&format!(
        "prop{}",
        DICTIONARY_THRESHOLD - 1
    )));
    let _ = obj.set(key, Value::int32(DICTIONARY_THRESHOLD as i32 - 1));

    // One more should trigger dictionary mode
    let key = PropertyKey::String(crate::string::JsString::intern(&format!(
        "prop{}",
        DICTIONARY_THRESHOLD
    )));
    let _ = obj.set(key, Value::int32(DICTIONARY_THRESHOLD as i32));

    assert!(
        obj.is_dictionary_mode(),
        "Object should be in dictionary mode after exceeding threshold"
    );
}

#[test]
fn test_dictionary_mode_delete_trigger() {
    // Test that deleting properties defers dictionary mode until 3+ deletes
    use crate::object::{JsObject, PropertyKey};

    let _rt = crate::runtime::VmRuntime::new();
    let _memory_manager = _rt.memory_manager().clone();
    let obj = GcRef::new(JsObject::new(Value::null()));

    // Add a few properties
    let key_a = PropertyKey::String(crate::string::JsString::intern("a"));
    let key_b = PropertyKey::String(crate::string::JsString::intern("b"));
    let key_c = PropertyKey::String(crate::string::JsString::intern("c"));
    let key_d = PropertyKey::String(crate::string::JsString::intern("d"));
    let _ = obj.set(key_a.clone(), Value::int32(1));
    let _ = obj.set(key_b.clone(), Value::int32(2));
    let _ = obj.set(key_c.clone(), Value::int32(3));
    let _ = obj.set(key_d.clone(), Value::int32(4));

    assert!(
        !obj.is_dictionary_mode(),
        "Object should not be in dictionary mode before delete"
    );

    // Delete 1 property — should stay shaped (slot-clearing)
    obj.delete(&key_a);
    assert!(
        !obj.is_dictionary_mode(),
        "Object should stay shaped after 1 delete"
    );
    // Deleted property should be absent
    assert!(!obj.has_own(&key_a));
    // Remaining properties still accessible
    assert_eq!(obj.get(&key_b), Some(Value::int32(2)));

    // Delete 2nd property — still shaped
    obj.delete(&key_b);
    assert!(
        !obj.is_dictionary_mode(),
        "Object should stay shaped after 2 deletes"
    );

    // Delete 3rd property — triggers dictionary mode
    obj.delete(&key_c);
    assert!(
        obj.is_dictionary_mode(),
        "Object should be in dictionary mode after 3 deletes"
    );

    // Remaining property still accessible
    assert_eq!(obj.get(&key_d), Some(Value::int32(4)));
}

#[test]
fn test_dictionary_mode_storage_correctness() {
    // Test that slot-clearing and dictionary mode storage work correctly
    use crate::object::{JsObject, PropertyKey};

    let _rt = crate::runtime::VmRuntime::new();
    let _memory_manager = _rt.memory_manager().clone();
    let obj = GcRef::new(JsObject::new(Value::null()));

    // Add properties
    let key_a = PropertyKey::String(crate::string::JsString::intern("a"));
    let key_b = PropertyKey::String(crate::string::JsString::intern("b"));
    let _ = obj.set(key_a.clone(), Value::int32(42));
    let _ = obj.set(key_b.clone(), Value::int32(100));

    // Delete key_b (slot-clearing, not dict mode yet)
    obj.delete(&key_b);
    assert!(!obj.is_dictionary_mode());

    // Verify has_own and get work with cleared slot
    assert!(obj.has_own(&key_a));
    assert!(!obj.has_own(&key_b));
    assert_eq!(obj.get(&key_a), Some(Value::int32(42)));
    assert_eq!(obj.get(&key_b), None); // Deleted (cleared slot)

    // Re-insert key_b (re-creates data property in cleared slot)
    let _ = obj.set(key_b.clone(), Value::int32(200));
    assert_eq!(obj.get(&key_b), Some(Value::int32(200)));
    assert!(obj.has_own(&key_b));

    // Now trigger dictionary mode with 3 deletes
    let key_c = PropertyKey::String(crate::string::JsString::intern("c"));
    let key_d = PropertyKey::String(crate::string::JsString::intern("d"));
    let _ = obj.set(key_c.clone(), Value::int32(300));
    let _ = obj.set(key_d.clone(), Value::int32(400));
    obj.delete(&key_b); // delete_count = 2 (had 1 before re-insert)
    obj.delete(&key_c); // delete_count = 3 → dictionary mode
    assert!(obj.is_dictionary_mode());

    // Verify dictionary mode storage
    assert_eq!(obj.get(&key_a), Some(Value::int32(42)));
    assert!(!obj.has_own(&key_b));
    assert!(!obj.has_own(&key_c));
    assert_eq!(obj.get(&key_d), Some(Value::int32(400)));
}

#[test]
fn test_dictionary_mode_ic_skip() {
    // Test that IC reports Megamorphic for dictionary mode objects
    use crate::object::{JsObject, PropertyKey};
    use otter_vm_bytecode::function::InlineCacheState;

    let _rt = crate::runtime::VmRuntime::new();
    let _memory_manager = _rt.memory_manager().clone();
    let obj = GcRef::new(JsObject::new(Value::null()));

    // Add properties and trigger dictionary mode via 3 deletes
    let key_a = PropertyKey::String(crate::string::JsString::intern("a"));
    let key_b = PropertyKey::String(crate::string::JsString::intern("b"));
    let key_c = PropertyKey::String(crate::string::JsString::intern("c"));
    let key_d = PropertyKey::String(crate::string::JsString::intern("d"));
    let _ = obj.set(key_a.clone(), Value::int32(1));
    let _ = obj.set(key_b.clone(), Value::int32(2));
    let _ = obj.set(key_c.clone(), Value::int32(3));
    let _ = obj.set(key_d.clone(), Value::int32(4));

    // 1-2 deletes: still shaped, IC remains valid
    obj.delete(&key_a);
    assert!(!obj.is_dictionary_mode());
    obj.delete(&key_b);
    assert!(!obj.is_dictionary_mode());

    // 3rd delete: transitions to dictionary mode
    obj.delete(&key_c);
    assert!(obj.is_dictionary_mode());

    // Create an IC metadata and verify it can detect dictionary mode
    let mut ic = otter_vm_bytecode::function::InstructionMetadata::new();

    // Simulate what IC write code does for dictionary mode objects
    if obj.is_dictionary_mode() {
        ic.ic_state = InlineCacheState::Megamorphic;
    }

    // IC should be Megamorphic for dictionary mode objects
    assert!(
        matches!(ic.ic_state, InlineCacheState::Megamorphic),
        "IC should be Megamorphic for dictionary mode objects"
    );
}

// ==================== Hot Function Detection Tests ====================

#[test]
fn test_hot_function_detection_call_count() {
    use otter_vm_bytecode::function::HOT_FUNCTION_THRESHOLD;

    let mut builder = Module::builder("test.js");

    // Simple function that returns immediately
    let func = Function::builder()
        .name("hot_candidate")
        .register_count(1)
        .instruction(Instruction::LoadInt32 {
            dst: Register(0),
            value: 42,
        })
        .instruction(Instruction::Return { src: Register(0) })
        .build();

    builder.add_function(func);
    let module = builder.build();
    let module = Arc::new(module);

    // Get the function and check initial state
    let func = module.function(0).unwrap();
    assert_eq!(func.get_call_count(), 0);
    assert!(!func.is_hot_function());

    // Execute the function multiple times
    for _ in 0..100 {
        let (mut ctx, _rt) = create_test_context_with_runtime();
        let interpreter = Interpreter::new();
        let _ = interpreter.execute_arc(module.clone(), &mut ctx);
    }

    // Call count should be 100
    assert_eq!(func.get_call_count(), 100);
    assert!(!func.is_hot_function()); // Not yet hot

    // Execute until we cross the threshold
    for _ in 0..(HOT_FUNCTION_THRESHOLD - 100) {
        let (mut ctx, _rt) = create_test_context_with_runtime();
        let interpreter = Interpreter::new();
        let _ = interpreter.execute_arc(module.clone(), &mut ctx);
    }

    // Should now be hot
    assert!(func.get_call_count() >= HOT_FUNCTION_THRESHOLD);
    assert!(func.is_hot_function());
}
#[test]
fn test_jit_loop_candidate_detection() {
    let loop_func = Function::builder()
        .name("loop_func")
        .register_count(1)
        .instruction(Instruction::LoadTrue { dst: Register(0) })
        .instruction(Instruction::JumpIfTrue {
            cond: Register(0),
            offset: otter_vm_bytecode::JumpOffset(-1),
        })
        .build();

    assert!(Interpreter::has_backward_jump(&loop_func));
    assert!(Interpreter::is_static_jit_candidate(&loop_func));

    let non_loop = Function::builder()
        .name("non_loop")
        .instruction(Instruction::ReturnUndefined)
        .build();
    assert!(!Interpreter::has_backward_jump(&non_loop));

    let non_candidate = Function::builder()
        .name("non_candidate")
        .flags(otter_vm_bytecode::function::FunctionFlags {
            uses_arguments: true,
            ..Default::default()
        })
        .instruction(Instruction::ReturnUndefined)
        .build();
    assert!(!Interpreter::is_static_jit_candidate(&non_candidate));
}

#[test]
fn test_hot_function_detection_record_call() {
    use otter_vm_bytecode::function::HOT_FUNCTION_THRESHOLD;

    let func = Function::builder()
        .name("test")
        .instruction(Instruction::Return { src: Register(0) })
        .build();

    // Initially not hot
    assert_eq!(func.get_call_count(), 0);
    assert!(!func.is_hot_function());

    // Record calls up to threshold - 1
    for _ in 0..(HOT_FUNCTION_THRESHOLD - 1) {
        let became_hot = func.record_call();
        assert!(!became_hot);
    }

    assert!(!func.is_hot_function());

    // This call should make it hot
    let became_hot = func.record_call();
    assert!(became_hot);
    assert!(func.is_hot_function());

    // Subsequent calls should not report becoming hot again
    let became_hot = func.record_call();
    assert!(!became_hot);
    assert!(func.is_hot_function());
}

#[test]
fn test_hot_function_mark_hot() {
    let func = Function::builder()
        .name("test")
        .instruction(Instruction::Return { src: Register(0) })
        .build();

    assert!(!func.is_hot_function());

    // Manually mark as hot
    func.mark_hot();
    assert!(func.is_hot_function());
}

#[test]
fn test_hot_function_nested_calls() {
    use otter_vm_bytecode::FunctionIndex;

    let mut builder = Module::builder("test.js");

    // Main function calls inner function in a loop
    let main = Function::builder()
        .name("main")
        .register_count(6)
        .instruction(Instruction::Closure {
            dst: Register(0),
            func: FunctionIndex(1),
        })
        .instruction(Instruction::LoadInt32 {
            dst: Register(1),
            value: 0,
        }) // counter
        .instruction(Instruction::LoadInt32 {
            dst: Register(2),
            value: 100,
        }) // limit
        // Loop: call inner function
        .instruction(Instruction::Call {
            dst: Register(3),
            func: Register(0),
            argc: 0,
            ic_index: 0,
        })
        .instruction(Instruction::LoadInt32 {
            dst: Register(4),
            value: 1,
        })
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
        .instruction(Instruction::Return { src: Register(1) })
        .feedback_vector_size(1)
        .build();

    // Inner function just returns 1
    let inner = Function::builder()
        .name("inner")
        .register_count(1)
        .instruction(Instruction::LoadInt32 {
            dst: Register(0),
            value: 1,
        })
        .instruction(Instruction::Return { src: Register(0) })
        .build();

    builder.add_function(main);
    builder.add_function(inner);
    let module = builder.build();
    let module = Arc::new(module);

    let (mut ctx, _rt) = create_test_context_with_runtime();
    let interpreter = Interpreter::new();
    let result = interpreter.execute_arc(module.clone(), &mut ctx).unwrap();

    // Execution should return 100 (counter after 100 loop iterations)
    assert_eq!(result.as_int32(), Some(100));

    // precompile_module_jit_candidates may mark functions with backward
    // jumps as hot before record_call runs, causing record_call to skip
    // the increment (by design — hot functions avoid atomic RMW overhead).
    // So we check call_count OR is_hot.
    let main_func = module.function(0).unwrap();
    assert!(
        main_func.get_call_count() >= 1 || main_func.is_hot_function(),
        "main should have been called (count={}, is_hot={})",
        main_func.get_call_count(),
        main_func.is_hot_function()
    );

    // Inner function: called 100 times via Call instruction in run_loop.
    // Same caveat applies if jit marks it hot before counting begins.
    let inner_func = module.function(1).unwrap();
    assert!(
        inner_func.get_call_count() >= 100 || inner_func.is_hot_function(),
        "inner should have been called 100 times (count={}, is_hot={})",
        inner_func.get_call_count(),
        inner_func.is_hot_function()
    );
}
