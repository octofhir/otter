//! Generator Frame Snapshot Tests
//!
//! These tests verify that the generator frame snapshot mechanism
//! correctly saves and restores execution state across yields.

use otter_vm_bytecode::Module;
use otter_vm_core::context::VmContext;
use otter_vm_core::gc::GcRef;
use otter_vm_core::generator::{GeneratorFrame, GeneratorState, JsGenerator};
use otter_vm_core::interpreter::{GeneratorResult, Interpreter};
use otter_vm_core::memory::MemoryManager;
use otter_vm_core::object::JsObject;
use otter_vm_core::value::Value;
use std::sync::Arc;

fn create_test_context() -> VmContext {
    let memory_manager = Arc::new(MemoryManager::test());
    let global = GcRef::new(JsObject::new(None, memory_manager.clone()));
    VmContext::new(global, memory_manager)
}

#[test]
fn test_generator_frame_creation() {
    // Test that GeneratorFrame can be created with all fields
    let module = Arc::new(Module::builder("test").build());
    let frame = GeneratorFrame::new(
        42, // pc
        0,  // function_index
        Arc::clone(&module),
        vec![Value::int32(1), Value::int32(2)],   // locals
        vec![Value::int32(10), Value::int32(20)], // registers
        vec![],                                   // upvalues
        vec![],                                   // try_stack
        Value::undefined(),                       // this_value
        false,                                    // is_construct
        1,                                        // frame_id
        0,                                        // argc
    );

    assert_eq!(frame.pc, 42);
    assert_eq!(frame.function_index, 0);
    assert_eq!(frame.locals.len(), 2);
    assert_eq!(frame.registers.len(), 2);
    assert_eq!(frame.frame_id, 1);
}

#[test]
fn test_generator_state_suspended_start() {
    // Test that a new generator starts in SuspendedStart state
    let module = Arc::new(Module::builder("test").build());
    let mm = Arc::new(MemoryManager::test());
    let obj = GcRef::new(JsObject::new(None, mm.clone()));
    let generator = JsGenerator::new(
        0,
        module,
        vec![],
        vec![],
        Value::undefined(),
        false,
        false,
        obj,
    );

    assert!(generator.is_suspended());
    assert!(generator.is_suspended_start());
    assert!(!generator.is_suspended_yield());
    assert!(!generator.is_executing());
    assert!(!generator.is_completed());
}

#[test]
fn test_generator_state_transitions() {
    // Test all generator state transitions
    let module = Arc::new(Module::builder("test").build());
    let mm = Arc::new(MemoryManager::test());
    let obj = GcRef::new(JsObject::new(None, mm.clone()));
    let generator = JsGenerator::new(
        0,
        Arc::clone(&module),
        vec![],
        vec![],
        Value::undefined(),
        false,
        false,
        obj,
    );

    // Initial state: SuspendedStart
    assert_eq!(generator.state(), GeneratorState::SuspendedStart);

    // Start executing
    generator.start_executing();
    assert_eq!(generator.state(), GeneratorState::Executing);
    assert!(generator.is_executing());

    // Suspend with yield
    let frame = GeneratorFrame::new(
        10,
        0,
        module,
        vec![],
        vec![],
        vec![],
        vec![],
        Value::undefined(),
        false,
        0,
        0,
    );
    generator.suspend_with_frame(frame);
    assert_eq!(generator.state(), GeneratorState::SuspendedYield);
    assert!(generator.is_suspended_yield());

    // Complete
    generator.complete();
    assert_eq!(generator.state(), GeneratorState::Completed);
    assert!(generator.is_completed());
}

#[test]
fn test_generator_frame_save_restore() {
    // Test that frame data is preserved correctly
    let module = Arc::new(Module::builder("test").build());
    let mm = Arc::new(MemoryManager::test());
    let obj = GcRef::new(JsObject::new(None, mm.clone()));
    let generator = JsGenerator::new(
        0,
        Arc::clone(&module),
        vec![],
        vec![],
        Value::undefined(),
        false,
        false,
        obj,
    );

    // Create a frame with specific values
    let frame = GeneratorFrame::new(
        100, // pc
        0,   // function_index
        Arc::clone(&module),
        vec![Value::int32(42), Value::int32(99)], // locals
        vec![Value::number(3.14)],                // registers
        vec![],                                   // upvalues
        vec![],                                   // try_stack
        Value::int32(1000),                       // this_value
        false,
        5, // frame_id
        2, // argc
    );

    // Suspend with frame
    generator.start_executing();
    generator.suspend_with_frame(frame);

    // Get the frame back
    let saved_frame = generator.get_frame().expect("Frame should exist");

    // Verify all fields
    assert_eq!(saved_frame.pc, 100);
    assert_eq!(saved_frame.function_index, 0);
    assert_eq!(saved_frame.locals.len(), 2);
    assert_eq!(saved_frame.locals[0].as_int32(), Some(42));
    assert_eq!(saved_frame.locals[1].as_int32(), Some(99));
    assert_eq!(saved_frame.registers.len(), 1);
    assert_eq!(saved_frame.registers[0].as_number(), Some(3.14));
    assert_eq!(saved_frame.this_value.as_int32(), Some(1000));
    assert_eq!(saved_frame.frame_id, 5);
}

#[test]
fn test_generator_sent_value() {
    // Test that sent values are stored and retrieved correctly
    let module = Arc::new(Module::builder("test").build());
    let mm = Arc::new(MemoryManager::test());
    let obj = GcRef::new(JsObject::new(None, mm.clone()));
    let generator = JsGenerator::new(
        0,
        Arc::clone(&module),
        vec![],
        vec![],
        Value::undefined(),
        false,
        false,
        obj,
    );

    // Create and suspend with frame
    let frame = GeneratorFrame::new(
        0,
        0,
        module,
        vec![],
        vec![],
        vec![],
        vec![],
        Value::undefined(),
        false,
        0,
        0,
    );
    generator.start_executing();
    generator.suspend_with_frame(frame);

    // Set sent value
    generator.set_sent_value(Value::int32(42));

    // Take sent value
    let value = generator.take_sent_value();
    assert!(value.is_some());
    assert_eq!(value.unwrap().as_int32(), Some(42));

    // Value should be consumed
    assert!(generator.take_sent_value().is_none());
}

#[test]
fn test_generator_pending_throw() {
    // Test pending throw functionality
    let module = Arc::new(Module::builder("test").build());
    let mm = Arc::new(MemoryManager::test());
    let obj = GcRef::new(JsObject::new(None, mm.clone()));
    let generator = JsGenerator::new(
        0,
        Arc::clone(&module),
        vec![],
        vec![],
        Value::undefined(),
        false,
        false,
        obj,
    );

    // Create and suspend with frame
    let frame = GeneratorFrame::new(
        0,
        0,
        module,
        vec![],
        vec![],
        vec![],
        vec![],
        Value::undefined(),
        false,
        0,
        0,
    );
    generator.start_executing();
    generator.suspend_with_frame(frame);

    // Set pending throw
    generator.set_pending_throw(Value::int32(500));

    // Take pending throw
    let error = generator.take_pending_throw();
    assert!(error.is_some());
    assert_eq!(error.unwrap().as_int32(), Some(500));

    // Should be consumed
    assert!(generator.take_pending_throw().is_none());
}

#[test]
fn test_generator_completion_type() {
    use otter_vm_core::generator::CompletionType;

    let module = Arc::new(Module::builder("test").build());
    let mm = Arc::new(MemoryManager::test());
    let obj = GcRef::new(JsObject::new(None, mm.clone()));
    let generator = JsGenerator::new(
        0,
        Arc::clone(&module),
        vec![],
        vec![],
        Value::undefined(),
        false,
        false,
        obj,
    );

    // Create and suspend with frame
    let frame = GeneratorFrame::new(
        0,
        0,
        module,
        vec![],
        vec![],
        vec![],
        vec![],
        Value::undefined(),
        false,
        0,
        0,
    );
    generator.start_executing();
    generator.suspend_with_frame(frame);

    // Default is Normal
    assert!(matches!(
        generator.completion_type(),
        CompletionType::Normal
    ));

    // Set Return completion
    generator.set_completion_type(CompletionType::Return(Value::int32(42)));
    if let CompletionType::Return(v) = generator.completion_type() {
        assert_eq!(v.as_int32(), Some(42));
    } else {
        panic!("Expected Return completion");
    }

    // Set Throw completion
    generator.set_completion_type(CompletionType::Throw(Value::int32(500)));
    if let CompletionType::Throw(v) = generator.completion_type() {
        assert_eq!(v.as_int32(), Some(500));
    } else {
        panic!("Expected Throw completion");
    }
}

#[test]
fn test_execute_generator_completed() {
    // Test that executing a completed generator returns Returned(undefined)
    let module = Arc::new(Module::builder("test").build());
    let mm = Arc::new(MemoryManager::test());
    let obj = GcRef::new(JsObject::new(None, mm));
    let generator = JsGenerator::new(
        0,
        module,
        vec![],
        vec![],
        Value::undefined(),
        false,
        false,
        obj,
    );

    // Complete the generator
    generator.complete();

    let mut ctx = create_test_context();
    let mut interpreter = Interpreter::new();

    let result = interpreter.execute_generator(&generator, &mut ctx, None);

    match result {
        GeneratorResult::Returned(v) => {
            assert!(v.is_undefined());
        }
        _ => panic!("Expected Returned result for completed generator"),
    }
}

#[test]
fn test_execute_generator_already_executing() {
    // Test that executing an already-executing generator returns Error
    let module = Arc::new(Module::builder("test").build());
    let mm = Arc::new(MemoryManager::test());
    let obj = GcRef::new(JsObject::new(None, mm));
    let generator = JsGenerator::new(
        0,
        module,
        vec![],
        vec![],
        Value::undefined(),
        false,
        false,
        obj,
    );

    // Mark as executing
    generator.start_executing();

    let mut ctx = create_test_context();
    let mut interpreter = Interpreter::new();

    let result = interpreter.execute_generator(&generator, &mut ctx, None);

    match result {
        GeneratorResult::Error(e) => {
            assert!(e.to_string().contains("already executing"));
        }
        _ => panic!("Expected Error result for executing generator"),
    }
}

#[test]
fn test_try_handler_serialization() {
    // Test that try handlers are properly saved and restored
    let mut ctx = create_test_context();

    // Push some try handlers
    // Note: When no frames are pushed, call_stack.len() == 0
    // So handlers pushed now will have frame_depth == 0
    ctx.push_try(100);
    ctx.push_try(200);

    // Get handlers for current frame (frame_depth 0 when no frames pushed)
    let handlers = ctx.get_try_handlers_for_current_frame();

    // Should have 2 handlers since call_stack.len() == 0 matches frame_depth == 0
    assert_eq!(handlers.len(), 2);
    assert_eq!(handlers[0], (100, 0)); // (catch_pc, frame_depth)
    assert_eq!(handlers[1], (200, 0));

    // Test restore
    let mut ctx2 = create_test_context();
    ctx2.restore_try_handlers(&handlers);

    let restored = ctx2.get_try_handlers_for_current_frame();
    assert_eq!(restored.len(), 2);
}
