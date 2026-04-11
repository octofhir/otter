//! Unit tests for the interpreter: register operations, arithmetic,
//! property access, call dispatch, closures, intrinsics, inherited accessors,
//! and other integration paths exercised through `Interpreter::run`.

    use crate::bigint::BigIntTable;
    use crate::bytecode::{Bytecode, BytecodeRegister, Instruction, JumpOffset};
    use crate::call::{CallSite, CallTable, ClosureCall, DirectCall};
    use crate::closure::{CaptureDescriptor, ClosureTable, ClosureTemplate, UpvalueId};
    use crate::deopt::DeoptTable;
    use crate::descriptors::{NativeFunctionDescriptor, VmNativeCallError};
    use crate::exception::ExceptionTable;
    use crate::feedback::{FeedbackKind, FeedbackSlotId, FeedbackSlotLayout, FeedbackTableLayout};
    use crate::float::FloatTable;
    use crate::frame::{FrameFlags, FrameLayout};
    use crate::intrinsics::WellKnownSymbol;
    use crate::module::{Function, FunctionIndex, FunctionSideTables, FunctionTables, Module};
    use crate::object::{HeapValueKind, ObjectHandle, PropertyValue};
    use crate::payload::{VmTrace, VmValueTracer};
    use crate::property::PropertyNameTable;
    use crate::source_map::SourceMap;
    use crate::string::StringTable;
    use crate::value::RegisterValue;

    use super::{Activation, ExecutionResult, Interpreter, InterpreterError, RuntimeState};

    fn inherited_accessor_getter(
        this: &RegisterValue,
        _args: &[RegisterValue],
        runtime: &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError> {
        let receiver = this
            .as_object_handle()
            .map(ObjectHandle)
            .ok_or_else(|| VmNativeCallError::Internal("expected object receiver".into()))?;
        let backing = runtime.intern_property_name("__backing");
        match runtime.objects().get_property(receiver, backing) {
            Ok(Some(lookup)) => match lookup.value() {
                PropertyValue::Data { value, .. } => Ok(value),
                PropertyValue::Accessor { .. } => Ok(RegisterValue::undefined()),
            },
            Ok(None) => Ok(RegisterValue::undefined()),
            Err(error) => Err(VmNativeCallError::Internal(
                format!("getter lookup failed: {error:?}").into(),
            )),
        }
    }

    fn inherited_accessor_setter(
        this: &RegisterValue,
        args: &[RegisterValue],
        runtime: &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError> {
        let receiver = this
            .as_object_handle()
            .map(ObjectHandle)
            .ok_or_else(|| VmNativeCallError::Internal("expected object receiver".into()))?;
        let backing = runtime.intern_property_name("__backing");
        let value = args
            .first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined);
        runtime
            .objects_mut()
            .set_property(receiver, backing, value)
            .map_err(|error| {
                VmNativeCallError::Internal(format!("setter store failed: {error:?}").into())
            })?;
        Ok(RegisterValue::undefined())
    }

    fn host_constructor_returns_primitive(
        this: &RegisterValue,
        _args: &[RegisterValue],
        runtime: &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError> {
        let receiver = this
            .as_object_handle()
            .map(ObjectHandle)
            .ok_or_else(|| VmNativeCallError::Internal("expected construct receiver".into()))?;
        let value = runtime.intern_property_name("value");
        runtime
            .objects_mut()
            .set_property(receiver, value, RegisterValue::from_i32(7))
            .map_err(|error| {
                VmNativeCallError::Internal(format!("constructor store failed: {error:?}").into())
            })?;
        Ok(RegisterValue::from_i32(1))
    }

    fn host_constructor_returns_object(
        _this: &RegisterValue,
        _args: &[RegisterValue],
        runtime: &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError> {
        let object = runtime.alloc_object();
        let value = runtime.intern_property_name("value");
        runtime
            .objects_mut()
            .set_property(object, value, RegisterValue::from_i32(9))
            .map_err(|error| {
                VmNativeCallError::Internal(format!("constructor store failed: {error:?}").into())
            })?;
        Ok(RegisterValue::from_object_handle(object.0))
    }

    fn host_plain_method(
        _this: &RegisterValue,
        _args: &[RegisterValue],
        _runtime: &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError> {
        Ok(RegisterValue::undefined())
    }

    fn host_echo_receiver(
        this: &RegisterValue,
        _args: &[RegisterValue],
        _runtime: &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError> {
        Ok(*this)
    }

    #[derive(Debug, Clone, PartialEq)]
    struct NativeCounterPayload {
        root: RegisterValue,
        shadow: Option<ObjectHandle>,
        calls: i32,
    }

    impl VmTrace for NativeCounterPayload {
        fn trace(&self, tracer: &mut dyn VmValueTracer) {
            self.root.trace(tracer);
            self.shadow.trace(tracer);
        }
    }

    #[derive(Default)]
    struct CollectingTracer {
        values: Vec<RegisterValue>,
    }

    impl VmValueTracer for CollectingTracer {
        fn mark_value(&mut self, value: RegisterValue) {
            self.values.push(value);
        }
    }

    fn native_payload_reads_root(
        this: &RegisterValue,
        _args: &[RegisterValue],
        runtime: &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError> {
        let payload = runtime
            .native_payload_from_value::<NativeCounterPayload>(this)
            .map_err(|error| VmNativeCallError::Internal(error.to_string().into()))?;
        Ok(payload.root)
    }

    fn native_payload_allocates_then_throws(
        this: &RegisterValue,
        args: &[RegisterValue],
        runtime: &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError> {
        let shadow = runtime.alloc_object();
        {
            let payload = runtime
                .native_payload_mut_from_value::<NativeCounterPayload>(this)
                .map_err(|error| VmNativeCallError::Internal(error.to_string().into()))?;
            payload.calls = payload.calls.saturating_add(1);
            payload.shadow = Some(shadow);
        }

        for index in 0..64 {
            let _ = runtime.alloc_string(format!("payload-temp-{index}"));
            let _ = runtime.alloc_object();
        }

        Err(VmNativeCallError::Thrown(
            args.first()
                .copied()
                .unwrap_or_else(RegisterValue::undefined),
        ))
    }

    #[test]
    fn runtime_native_objects_expose_typed_payload_access() {
        let mut runtime = RuntimeState::new();
        let root = runtime.alloc_string("payload-root");
        let instance = runtime.alloc_native_object(NativeCounterPayload {
            root: RegisterValue::from_object_handle(root.0),
            shadow: None,
            calls: 0,
        });

        let payload = runtime
            .native_payload::<NativeCounterPayload>(instance)
            .expect("payload should downcast");
        assert_eq!(payload.root, RegisterValue::from_object_handle(root.0));
        assert_eq!(payload.calls, 0);

        let method = runtime.register_native_function(NativeFunctionDescriptor::method(
            "readRoot",
            0,
            native_payload_reads_root,
        ));
        let descriptor = runtime
            .native_functions()
            .get(method)
            .cloned()
            .expect("native descriptor should exist");
        let value = (descriptor.callback())(
            &RegisterValue::from_object_handle(instance.0),
            &[],
            &mut runtime,
        )
        .expect("native payload method should succeed");

        assert!(
            runtime
                .objects()
                .native_payload_id(instance)
                .expect("native payload lookup should succeed")
                .is_some()
        );
        assert_eq!(runtime.objects().kind(instance), Ok(HeapValueKind::Object));
        assert_eq!(
            runtime
                .objects()
                .strict_eq(value, RegisterValue::from_object_handle(root.0)),
            Ok(true)
        );
    }

    #[test]
    fn runtime_native_payload_tracing_survives_allocation_and_throw_pressure() {
        let mut runtime = RuntimeState::new();
        let root = runtime.alloc_string("root");
        let instance = runtime.alloc_native_object(NativeCounterPayload {
            root: RegisterValue::from_object_handle(root.0),
            shadow: None,
            calls: 0,
        });

        let thrower = runtime.register_native_function(NativeFunctionDescriptor::method(
            "explode",
            1,
            native_payload_allocates_then_throws,
        ));
        let descriptor = runtime
            .native_functions()
            .get(thrower)
            .cloned()
            .expect("throwing descriptor should exist");
        let thrown = RegisterValue::from_i32(9);
        let error = (descriptor.callback())(
            &RegisterValue::from_object_handle(instance.0),
            &[thrown],
            &mut runtime,
        )
        .expect_err("throwing callback should propagate abrupt completion");
        assert_eq!(error, VmNativeCallError::Thrown(thrown));

        let payload = runtime
            .native_payload::<NativeCounterPayload>(instance)
            .expect("payload should still be readable after throw");
        assert_eq!(payload.calls, 1);
        let shadow = payload
            .shadow
            .expect("throwing callback should store shadow root");

        let mut tracer = CollectingTracer::default();
        runtime
            .trace_native_payload_roots(&mut tracer)
            .expect("payload trace should succeed");
        assert!(
            tracer
                .values
                .contains(&RegisterValue::from_object_handle(root.0))
        );
        assert!(
            tracer
                .values
                .contains(&RegisterValue::from_object_handle(shadow.0))
        );
    }

    #[test]
    fn interpreter_executes_nop_then_return() {
        let layout = FrameLayout::new(0, 1, 0, 0).expect("frame layout should be valid");
        let function = Function::with_bytecode(
            Some("entry"),
            layout,
            Bytecode::from(vec![
                Instruction::nop(),
                Instruction::ret(BytecodeRegister::new(0)),
            ]),
        );
        let module = Module::new(Some("m"), vec![function], FunctionIndex(0))
            .expect("module should be valid");
        let interpreter = Interpreter::new();
        let mut activation = Interpreter::prepare_entry(&module);
        activation
            .set_register(layout.user_visible_start(), RegisterValue::from_i32(7))
            .expect("register should exist");

        let result = interpreter.run(&module, &mut activation);

        assert_eq!(result, Ok(ExecutionResult::new(RegisterValue::from_i32(7))));
        assert_eq!(activation.pc(), 1);
    }

    #[test]
    fn interpreter_executes_arithmetic_program() {
        let layout = FrameLayout::new(1, 0, 0, 7).expect("frame layout should be valid");
        let function = Function::with_bytecode(
            Some("entry"),
            layout,
            Bytecode::from(vec![
                Instruction::load_i32(BytecodeRegister::new(0), 20),
                Instruction::load_i32(BytecodeRegister::new(1), 22),
                Instruction::add(
                    BytecodeRegister::new(2),
                    BytecodeRegister::new(0),
                    BytecodeRegister::new(1),
                ),
                Instruction::sub(
                    BytecodeRegister::new(3),
                    BytecodeRegister::new(2),
                    BytecodeRegister::new(0),
                ),
                Instruction::mul(
                    BytecodeRegister::new(4),
                    BytecodeRegister::new(3),
                    BytecodeRegister::new(1),
                ),
                Instruction::load_i32(BytecodeRegister::new(5), 2),
                Instruction::div(
                    BytecodeRegister::new(6),
                    BytecodeRegister::new(4),
                    BytecodeRegister::new(5),
                ),
                Instruction::ret(BytecodeRegister::new(6)),
            ]),
        );
        let module = Module::new(Some("m"), vec![function], FunctionIndex(0))
            .expect("module should be valid");

        let result = Interpreter::new().execute(&module);

        assert_eq!(
            result.map(ExecutionResult::return_value),
            Ok(RegisterValue::from_i32(242))
        );
    }

    #[test]
    fn interpreter_reports_unexpected_end_of_bytecode() {
        let function =
            Function::with_bytecode(Some("entry"), FrameLayout::default(), Bytecode::default());
        let module = Module::new(Some("m"), vec![function], FunctionIndex(0))
            .expect("module should be valid");

        let result = Interpreter::new().execute(&module);

        assert_eq!(result, Err(InterpreterError::UnexpectedEndOfBytecode));
    }

    #[test]
    fn interpreter_executes_loop_with_conditional_branch() {
        let layout = FrameLayout::new(0, 0, 0, 5).expect("frame layout should be valid");
        let function = Function::with_bytecode(
            Some("entry"),
            layout,
            Bytecode::from(vec![
                Instruction::load_i32(BytecodeRegister::new(0), 0),
                Instruction::load_i32(BytecodeRegister::new(1), 4),
                Instruction::load_i32(BytecodeRegister::new(2), 0),
                Instruction::load_i32(BytecodeRegister::new(3), 1),
                Instruction::lt(
                    BytecodeRegister::new(4),
                    BytecodeRegister::new(0),
                    BytecodeRegister::new(1),
                ),
                Instruction::jump_if_false(BytecodeRegister::new(4), JumpOffset::new(3)),
                Instruction::add(
                    BytecodeRegister::new(2),
                    BytecodeRegister::new(2),
                    BytecodeRegister::new(0),
                ),
                Instruction::add(
                    BytecodeRegister::new(0),
                    BytecodeRegister::new(0),
                    BytecodeRegister::new(3),
                ),
                Instruction::jump(JumpOffset::new(-5)),
                Instruction::ret(BytecodeRegister::new(2)),
            ]),
        );
        let module = Module::new(Some("loop"), vec![function], FunctionIndex(0))
            .expect("module should be valid");

        let result = Interpreter::new().execute(&module);

        assert_eq!(
            result.map(ExecutionResult::return_value),
            Ok(RegisterValue::from_i32(6))
        );
    }

    #[test]
    fn interpreter_rejects_invalid_jump_target() {
        let layout = FrameLayout::new(0, 0, 0, 1).expect("frame layout should be valid");
        let function = Function::with_bytecode(
            Some("entry"),
            layout,
            Bytecode::from(vec![Instruction::jump(JumpOffset::new(-2))]),
        );
        let module = Module::new(Some("invalid-jump"), vec![function], FunctionIndex(0))
            .expect("module should be valid");

        let result = Interpreter::new().execute(&module);

        assert_eq!(result, Err(InterpreterError::InvalidJumpTarget));
    }

    #[test]
    fn interpreter_adds_boolean_and_number() {
        // ES2024 §13.15.3: `true + 1` → ToNumber(true) + 1 = 2.
        let layout = FrameLayout::new(0, 0, 0, 3).expect("frame layout should be valid");
        let function = Function::with_bytecode(
            Some("entry"),
            layout,
            Bytecode::from(vec![
                Instruction::load_true(BytecodeRegister::new(0)),
                Instruction::load_i32(BytecodeRegister::new(1), 1),
                Instruction::add(
                    BytecodeRegister::new(2),
                    BytecodeRegister::new(0),
                    BytecodeRegister::new(1),
                ),
                Instruction::ret(BytecodeRegister::new(2)),
            ]),
        );
        let module = Module::new(Some("bool-add"), vec![function], FunctionIndex(0))
            .expect("module should be valid");

        let result = Interpreter::new().execute(&module);
        assert!(result.is_ok(), "true + 1 should succeed");
    }

    #[test]
    fn interpreter_executes_object_property_round_trip() {
        let layout = FrameLayout::new(0, 0, 0, 4).expect("frame layout should be valid");
        let bytecode = Bytecode::from(vec![
            Instruction::new_object(BytecodeRegister::new(0)),
            Instruction::load_i32(BytecodeRegister::new(1), 7),
            Instruction::set_property(
                BytecodeRegister::new(0),
                BytecodeRegister::new(1),
                crate::property::PropertyNameId(0),
            ),
            Instruction::get_property(
                BytecodeRegister::new(2),
                BytecodeRegister::new(0),
                crate::property::PropertyNameId(0),
            ),
            Instruction::ret(BytecodeRegister::new(2)),
        ]);
        let feedback = FeedbackTableLayout::new(vec![
            FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Property),
            FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Property),
            FeedbackSlotLayout::new(FeedbackSlotId(2), FeedbackKind::Property),
            FeedbackSlotLayout::new(FeedbackSlotId(3), FeedbackKind::Property),
        ]);
        let function = Function::new(
            Some("entry"),
            layout,
            bytecode,
            FunctionTables::new(
                FunctionSideTables::new(
                    PropertyNameTable::new(vec!["count"]),
                    StringTable::default(),
                    FloatTable::default(),
                    BigIntTable::default(),
                    ClosureTable::default(),
                    CallTable::default(),
                    crate::regexp::RegExpTable::default(),
                ),
                feedback,
                DeoptTable::default(),
                ExceptionTable::default(),
                SourceMap::default(),
            ),
        );
        let module = Module::new(Some("object"), vec![function], FunctionIndex(0))
            .expect("module should be valid");

        let result = Interpreter::new().execute(&module);

        assert_eq!(
            result.map(ExecutionResult::return_value),
            Ok(RegisterValue::from_i32(7))
        );
    }

    #[test]
    fn interpreter_rejects_invalid_object_value() {
        let layout = FrameLayout::new(0, 0, 0, 3).expect("frame layout should be valid");
        let function = Function::new(
            Some("entry"),
            layout,
            Bytecode::from(vec![
                Instruction::load_undefined(BytecodeRegister::new(0)),
                Instruction::get_property(
                    BytecodeRegister::new(1),
                    BytecodeRegister::new(0),
                    crate::property::PropertyNameId(0),
                ),
            ]),
            FunctionTables::new(
                FunctionSideTables::new(
                    PropertyNameTable::new(vec!["count"]),
                    StringTable::default(),
                    FloatTable::default(),
                    BigIntTable::default(),
                    ClosureTable::default(),
                    CallTable::default(),
                    crate::regexp::RegExpTable::default(),
                ),
                FeedbackTableLayout::new(vec![
                    FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Property),
                ]),
                DeoptTable::default(),
                ExceptionTable::default(),
                SourceMap::default(),
            ),
        );
        let module = Module::new(Some("object"), vec![function], FunctionIndex(0))
            .expect("module should be valid");

        let result = Interpreter::new().execute(&module);
        assert!(matches!(result, Err(InterpreterError::UncaughtThrow(_))));
    }

    #[test]
    fn interpreter_executes_string_and_array_fast_paths() {
        let layout = FrameLayout::new(0, 0, 0, 10).expect("frame layout should be valid");
        let bytecode = Bytecode::from(vec![
            Instruction::load_string(BytecodeRegister::new(0), crate::string::StringId(0)),
            Instruction::get_property(
                BytecodeRegister::new(1),
                BytecodeRegister::new(0),
                crate::property::PropertyNameId(0),
            ),
            Instruction::new_array(BytecodeRegister::new(2), 0),
            Instruction::load_i32(BytecodeRegister::new(3), 0),
            Instruction::set_index(
                BytecodeRegister::new(2),
                BytecodeRegister::new(3),
                BytecodeRegister::new(1),
            ),
            Instruction::load_i32(BytecodeRegister::new(4), 1),
            Instruction::get_index(
                BytecodeRegister::new(5),
                BytecodeRegister::new(0),
                BytecodeRegister::new(4),
            ),
            Instruction::set_index(
                BytecodeRegister::new(2),
                BytecodeRegister::new(4),
                BytecodeRegister::new(5),
            ),
            Instruction::get_index(
                BytecodeRegister::new(6),
                BytecodeRegister::new(2),
                BytecodeRegister::new(3),
            ),
            Instruction::get_property(
                BytecodeRegister::new(7),
                BytecodeRegister::new(2),
                crate::property::PropertyNameId(0),
            ),
            Instruction::add(
                BytecodeRegister::new(8),
                BytecodeRegister::new(6),
                BytecodeRegister::new(7),
            ),
            Instruction::ret(BytecodeRegister::new(8)),
        ]);
        let function = Function::new(
            Some("entry"),
            layout,
            bytecode,
            FunctionTables::new(
                FunctionSideTables::new(
                    PropertyNameTable::new(vec!["length"]),
                    StringTable::new(vec!["otter"]),
                    FloatTable::default(),
                    BigIntTable::default(),
                    ClosureTable::default(),
                    CallTable::default(),
                    crate::regexp::RegExpTable::default(),
                ),
                FeedbackTableLayout::new(vec![
                    FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(2), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(3), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(4), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(5), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(6), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(7), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(8), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(9), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(10), FeedbackKind::Arithmetic),
                    FeedbackSlotLayout::new(FeedbackSlotId(11), FeedbackKind::Call),
                ]),
                DeoptTable::default(),
                ExceptionTable::default(),
                SourceMap::default(),
            ),
        );
        let module = Module::new(Some("string-array"), vec![function], FunctionIndex(0))
            .expect("module should be valid");

        let result = Interpreter::new().execute(&module);

        assert_eq!(
            result.map(ExecutionResult::return_value),
            Ok(RegisterValue::from_i32(7))
        );
    }

    #[test]
    fn interpreter_executes_direct_call_with_contiguous_argument_window() {
        let entry_layout = FrameLayout::new(0, 0, 0, 4).expect("frame layout should be valid");
        let helper_layout = FrameLayout::new(0, 2, 0, 1).expect("frame layout should be valid");
        let entry = Function::new(
            Some("entry"),
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
                    ClosureTable::default(),
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
                    crate::regexp::RegExpTable::default(),
                ),
                FeedbackTableLayout::new(vec![
                    FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(2), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(3), FeedbackKind::Call),
                ]),
                DeoptTable::default(),
                ExceptionTable::default(),
                SourceMap::default(),
            ),
        );
        let helper = Function::with_bytecode(
            Some("helper"),
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
        let module = Module::new(Some("direct-call"), vec![entry, helper], FunctionIndex(0))
            .expect("module should be valid");

        let result = Interpreter::new().execute(&module);

        assert_eq!(
            result.map(ExecutionResult::return_value),
            Ok(RegisterValue::from_i32(42))
        );
    }

    #[test]
    fn interpreter_shares_property_names_across_function_tables() {
        let entry_layout = FrameLayout::new(0, 0, 0, 3).expect("frame layout should be valid");
        let helper_layout = FrameLayout::new(0, 1, 0, 1).expect("frame layout should be valid");
        let entry = Function::new(
            Some("entry"),
            entry_layout,
            Bytecode::from(vec![
                Instruction::new_object(BytecodeRegister::new(0)),
                Instruction::load_i32(BytecodeRegister::new(1), 7),
                Instruction::set_property(
                    BytecodeRegister::new(0),
                    BytecodeRegister::new(1),
                    crate::property::PropertyNameId(1),
                ),
                Instruction::call_direct(BytecodeRegister::new(2), BytecodeRegister::new(0)),
                Instruction::ret(BytecodeRegister::new(2)),
            ]),
            FunctionTables::new(
                FunctionSideTables::new(
                    PropertyNameTable::new(vec!["ignored", "shared"]),
                    StringTable::default(),
                    FloatTable::default(),
                    BigIntTable::default(),
                    ClosureTable::default(),
                    CallTable::new(vec![
                        None,
                        None,
                        None,
                        Some(CallSite::Direct(DirectCall::new(
                            FunctionIndex(1),
                            1,
                            FrameFlags::empty(),
                        ))),
                        None,
                    ]),
                    crate::regexp::RegExpTable::default(),
                ),
                FeedbackTableLayout::new(vec![
                    FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(2), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(3), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(4), FeedbackKind::Call),
                ]),
                DeoptTable::default(),
                ExceptionTable::default(),
                SourceMap::default(),
            ),
        );
        let helper = Function::new(
            Some("helper"),
            helper_layout,
            Bytecode::from(vec![
                Instruction::get_property(
                    BytecodeRegister::new(1),
                    BytecodeRegister::new(0),
                    crate::property::PropertyNameId(0),
                ),
                Instruction::ret(BytecodeRegister::new(1)),
            ]),
            FunctionTables::new(
                FunctionSideTables::new(
                    PropertyNameTable::new(vec!["shared"]),
                    StringTable::default(),
                    FloatTable::default(),
                    BigIntTable::default(),
                    ClosureTable::default(),
                    CallTable::default(),
                    crate::regexp::RegExpTable::default(),
                ),
                FeedbackTableLayout::new(vec![
                    FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Call),
                ]),
                DeoptTable::default(),
                ExceptionTable::default(),
                SourceMap::default(),
            ),
        );
        let module = Module::new(
            Some("cross-function-property"),
            vec![entry, helper],
            FunctionIndex(0),
        )
        .expect("module should be valid");

        let result = Interpreter::new().execute(&module);

        assert_eq!(
            result.map(ExecutionResult::return_value),
            Ok(RegisterValue::from_i32(7))
        );
    }

    #[test]
    fn interpreter_calls_bootstrap_installed_math_abs() {
        let layout = FrameLayout::new(0, 0, 0, 5).expect("frame layout should be valid");
        let entry = Function::new(
            Some("entry"),
            layout,
            Bytecode::from(vec![
                Instruction::get_property(
                    BytecodeRegister::new(1),
                    BytecodeRegister::new(0),
                    crate::property::PropertyNameId(0),
                ),
                Instruction::get_property(
                    BytecodeRegister::new(2),
                    BytecodeRegister::new(1),
                    crate::property::PropertyNameId(1),
                ),
                Instruction::load_i32(BytecodeRegister::new(3), -7),
                Instruction::call_closure(
                    BytecodeRegister::new(4),
                    BytecodeRegister::new(2),
                    BytecodeRegister::new(3),
                ),
                Instruction::ret(BytecodeRegister::new(4)),
            ]),
            FunctionTables::new(
                FunctionSideTables::new(
                    PropertyNameTable::new(vec!["Math", "abs"]),
                    StringTable::default(),
                    FloatTable::default(),
                    BigIntTable::default(),
                    ClosureTable::default(),
                    CallTable::new(vec![
                        None,
                        None,
                        None,
                        Some(CallSite::Closure(ClosureCall::new_with_receiver(
                            1,
                            FrameFlags::new(false, true, false),
                            BytecodeRegister::new(1),
                        ))),
                        None,
                    ]),
                    crate::regexp::RegExpTable::default(),
                ),
                FeedbackTableLayout::new(vec![
                    FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(2), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(3), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(4), FeedbackKind::Call),
                ]),
                DeoptTable::default(),
                ExceptionTable::default(),
                SourceMap::default(),
            ),
        );
        let module = Module::new(Some("math-abs"), vec![entry], FunctionIndex(0))
            .expect("module should be valid");
        let mut runtime = RuntimeState::new();
        let global = runtime.intrinsics().global_object();
        let registers = [RegisterValue::from_object_handle(global.0)];

        let result = Interpreter::new().execute_with_runtime(
            &module,
            FunctionIndex(0),
            &registers,
            &mut runtime,
        );

        assert_eq!(
            result.map(ExecutionResult::return_value),
            Ok(RegisterValue::from_i32(7))
        );
    }

    #[test]
    fn interpreter_reads_and_writes_bootstrap_installed_math_accessor() {
        let layout = FrameLayout::new(0, 0, 0, 4).expect("frame layout should be valid");
        let entry = Function::new(
            Some("entry"),
            layout,
            Bytecode::from(vec![
                Instruction::get_property(
                    BytecodeRegister::new(1),
                    BytecodeRegister::new(0),
                    crate::property::PropertyNameId(0),
                ),
                Instruction::load_i32(BytecodeRegister::new(2), 7),
                Instruction::set_property(
                    BytecodeRegister::new(1),
                    BytecodeRegister::new(2),
                    crate::property::PropertyNameId(1),
                ),
                Instruction::get_property(
                    BytecodeRegister::new(3),
                    BytecodeRegister::new(1),
                    crate::property::PropertyNameId(1),
                ),
                Instruction::ret(BytecodeRegister::new(3)),
            ]),
            FunctionTables::new(
                FunctionSideTables::new(
                    PropertyNameTable::new(vec!["Math", "memory"]),
                    StringTable::default(),
                    FloatTable::default(),
                    BigIntTable::default(),
                    ClosureTable::default(),
                    CallTable::default(),
                    crate::regexp::RegExpTable::default(),
                ),
                FeedbackTableLayout::new(vec![
                    FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(2), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(3), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(4), FeedbackKind::Call),
                ]),
                DeoptTable::default(),
                ExceptionTable::default(),
                SourceMap::default(),
            ),
        );
        let module = Module::new(Some("math-accessor"), vec![entry], FunctionIndex(0))
            .expect("module should be valid");
        let mut runtime = RuntimeState::new();
        let global = runtime.intrinsics().global_object();
        let registers = [RegisterValue::from_object_handle(global.0)];

        let result = Interpreter::new().execute_with_runtime(
            &module,
            FunctionIndex(0),
            &registers,
            &mut runtime,
        );

        assert_eq!(
            result.map(ExecutionResult::return_value),
            Ok(RegisterValue::from_i32(7))
        );
    }

    #[test]
    fn interpreter_calls_bootstrap_installed_object_static_and_prototype_methods() {
        let layout = FrameLayout::new(0, 0, 0, 7).expect("frame layout should be valid");
        let entry = Function::new(
            Some("entry"),
            layout,
            Bytecode::from(vec![
                Instruction::get_property(
                    BytecodeRegister::new(1),
                    BytecodeRegister::new(0),
                    crate::property::PropertyNameId(0),
                ),
                Instruction::get_property(
                    BytecodeRegister::new(2),
                    BytecodeRegister::new(1),
                    crate::property::PropertyNameId(1),
                ),
                Instruction::call_closure(
                    BytecodeRegister::new(3),
                    BytecodeRegister::new(2),
                    BytecodeRegister::new(0),
                ),
                Instruction::get_property(
                    BytecodeRegister::new(4),
                    BytecodeRegister::new(3),
                    crate::property::PropertyNameId(2),
                ),
                Instruction::call_closure(
                    BytecodeRegister::new(5),
                    BytecodeRegister::new(4),
                    BytecodeRegister::new(3),
                ),
                Instruction::eq(
                    BytecodeRegister::new(6),
                    BytecodeRegister::new(5),
                    BytecodeRegister::new(3),
                ),
                Instruction::ret(BytecodeRegister::new(6)),
            ]),
            FunctionTables::new(
                FunctionSideTables::new(
                    PropertyNameTable::new(vec!["Object", "create", "valueOf"]),
                    StringTable::default(),
                    FloatTable::default(),
                    BigIntTable::default(),
                    ClosureTable::default(),
                    CallTable::new(vec![
                        None,
                        None,
                        Some(CallSite::Closure(ClosureCall::new_with_receiver(
                            1,
                            FrameFlags::new(false, true, false),
                            BytecodeRegister::new(1),
                        ))),
                        None,
                        Some(CallSite::Closure(ClosureCall::new_with_receiver(
                            0,
                            FrameFlags::new(false, true, false),
                            BytecodeRegister::new(3),
                        ))),
                        None,
                    ]),
                    crate::regexp::RegExpTable::default(),
                ),
                FeedbackTableLayout::new(vec![
                    FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(2), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(3), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(4), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(5), FeedbackKind::Comparison),
                ]),
                DeoptTable::default(),
                ExceptionTable::default(),
                SourceMap::default(),
            ),
        );
        let module = Module::new(Some("object-bootstrap"), vec![entry], FunctionIndex(0))
            .expect("module should be valid");
        let mut runtime = RuntimeState::new();
        let global = runtime.intrinsics().global_object();
        let registers = [RegisterValue::from_object_handle(global.0)];

        let result = Interpreter::new().execute_with_runtime(
            &module,
            FunctionIndex(0),
            &registers,
            &mut runtime,
        );

        assert_eq!(
            result.map(ExecutionResult::return_value),
            Ok(RegisterValue::from_bool(true))
        );
    }

    #[test]
    fn interpreter_calls_bootstrap_installed_function_static_and_prototype_methods() {
        let layout = FrameLayout::new(0, 0, 0, 10).expect("frame layout should be valid");
        let entry = Function::new(
            Some("entry"),
            layout,
            Bytecode::from(vec![
                Instruction::get_property(
                    BytecodeRegister::new(1),
                    BytecodeRegister::new(0),
                    crate::property::PropertyNameId(0),
                ),
                Instruction::get_property(
                    BytecodeRegister::new(2),
                    BytecodeRegister::new(1),
                    crate::property::PropertyNameId(1),
                ),
                Instruction::get_property(
                    BytecodeRegister::new(3),
                    BytecodeRegister::new(0),
                    crate::property::PropertyNameId(2),
                ),
                Instruction::get_property(
                    BytecodeRegister::new(4),
                    BytecodeRegister::new(3),
                    crate::property::PropertyNameId(3),
                ),
                Instruction::call_closure(
                    BytecodeRegister::new(5),
                    BytecodeRegister::new(4),
                    BytecodeRegister::new(2),
                ),
                Instruction::jump_if_false(BytecodeRegister::new(5), JumpOffset::new(6)),
                Instruction::get_property(
                    BytecodeRegister::new(6),
                    BytecodeRegister::new(2),
                    crate::property::PropertyNameId(4),
                ),
                Instruction::call_closure(
                    BytecodeRegister::new(7),
                    BytecodeRegister::new(6),
                    BytecodeRegister::new(2),
                ),
                Instruction::load_string(BytecodeRegister::new(8), crate::string::StringId(0)),
                Instruction::eq(
                    BytecodeRegister::new(9),
                    BytecodeRegister::new(7),
                    BytecodeRegister::new(8),
                ),
                Instruction::ret(BytecodeRegister::new(9)),
                Instruction::load_false(BytecodeRegister::new(9)),
                Instruction::ret(BytecodeRegister::new(9)),
            ]),
            FunctionTables::new(
                FunctionSideTables::new(
                    PropertyNameTable::new(vec![
                        "Math",
                        "abs",
                        "Function",
                        "isCallable",
                        "toString",
                    ]),
                    StringTable::new(vec!["function () { [native code] }"]),
                    FloatTable::default(),
                    BigIntTable::default(),
                    ClosureTable::default(),
                    CallTable::new(vec![
                        None,
                        None,
                        None,
                        None,
                        Some(CallSite::Closure(ClosureCall::new_with_receiver(
                            1,
                            FrameFlags::new(false, true, false),
                            BytecodeRegister::new(3),
                        ))),
                        None,
                        None,
                        Some(CallSite::Closure(ClosureCall::new_with_receiver(
                            0,
                            FrameFlags::new(false, true, false),
                            BytecodeRegister::new(2),
                        ))),
                        None,
                        None,
                        None,
                    ]),
                    crate::regexp::RegExpTable::default(),
                ),
                FeedbackTableLayout::new(vec![
                    FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(2), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(3), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(4), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(5), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(6), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(7), FeedbackKind::Comparison),
                ]),
                DeoptTable::default(),
                ExceptionTable::default(),
                SourceMap::default(),
            ),
        );
        let module = Module::new(Some("function-bootstrap"), vec![entry], FunctionIndex(0))
            .expect("module should be valid");
        let mut runtime = RuntimeState::new();
        let global = runtime.intrinsics().global_object();
        let registers = [RegisterValue::from_object_handle(global.0)];

        let result = Interpreter::new().execute_with_runtime(
            &module,
            FunctionIndex(0),
            &registers,
            &mut runtime,
        );

        assert_eq!(
            result.map(ExecutionResult::return_value),
            Ok(RegisterValue::from_bool(true))
        );
    }

    #[test]
    fn interpreter_set_property_creates_own_data_slot_when_property_is_inherited() {
        let layout = FrameLayout::new(0, 0, 0, 3).expect("frame layout should be valid");
        let entry = Function::new(
            Some("entry"),
            layout,
            Bytecode::from(vec![
                Instruction::load_i32(BytecodeRegister::new(1), 7),
                Instruction::set_property(
                    BytecodeRegister::new(0),
                    BytecodeRegister::new(1),
                    crate::property::PropertyNameId(0),
                ),
                Instruction::get_property(
                    BytecodeRegister::new(2),
                    BytecodeRegister::new(0),
                    crate::property::PropertyNameId(0),
                ),
                Instruction::ret(BytecodeRegister::new(2)),
            ]),
            FunctionTables::new(
                FunctionSideTables::new(
                    PropertyNameTable::new(vec!["value"]),
                    StringTable::default(),
                    FloatTable::default(),
                    BigIntTable::default(),
                    ClosureTable::default(),
                    CallTable::default(),
                    crate::regexp::RegExpTable::default(),
                ),
                FeedbackTableLayout::new(vec![
                    FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(2), FeedbackKind::Property),
                ]),
                DeoptTable::default(),
                ExceptionTable::default(),
                SourceMap::default(),
            ),
        );
        let module = Module::new(Some("inherited-data-set"), vec![entry], FunctionIndex(0))
            .expect("module should be valid");
        let mut runtime = RuntimeState::new();
        let prototype = runtime.alloc_object();
        let object = runtime.alloc_object_with_prototype(Some(prototype));
        let property = runtime.intern_property_name("value");
        runtime
            .objects_mut()
            .set_property(prototype, property, RegisterValue::from_i32(1))
            .expect("prototype data property should install");
        let registers = [RegisterValue::from_object_handle(object.0)];

        let result = Interpreter::new().execute_with_runtime(
            &module,
            FunctionIndex(0),
            &registers,
            &mut runtime,
        );

        assert_eq!(
            result.map(ExecutionResult::return_value),
            Ok(RegisterValue::from_i32(7))
        );
        let object_lookup = runtime
            .objects()
            .get_property(object, property)
            .expect("receiver lookup should succeed")
            .expect("receiver value should exist");
        assert_eq!(object_lookup.owner(), object);
        assert_eq!(
            object_lookup.value(),
            PropertyValue::data(RegisterValue::from_i32(7))
        );
        let prototype_lookup = runtime
            .objects()
            .get_property(prototype, property)
            .expect("prototype lookup should succeed")
            .expect("prototype value should exist");
        assert_eq!(prototype_lookup.owner(), prototype);
        assert_eq!(
            prototype_lookup.value(),
            PropertyValue::data(RegisterValue::from_i32(1))
        );
    }

    #[test]
    fn interpreter_set_property_invokes_inherited_accessor_setter() {
        let layout = FrameLayout::new(0, 0, 0, 3).expect("frame layout should be valid");
        let entry = Function::new(
            Some("entry"),
            layout,
            Bytecode::from(vec![
                Instruction::load_i32(BytecodeRegister::new(1), 7),
                Instruction::set_property(
                    BytecodeRegister::new(0),
                    BytecodeRegister::new(1),
                    crate::property::PropertyNameId(0),
                ),
                Instruction::get_property(
                    BytecodeRegister::new(2),
                    BytecodeRegister::new(0),
                    crate::property::PropertyNameId(0),
                ),
                Instruction::ret(BytecodeRegister::new(2)),
            ]),
            FunctionTables::new(
                FunctionSideTables::new(
                    PropertyNameTable::new(vec!["value"]),
                    StringTable::default(),
                    FloatTable::default(),
                    BigIntTable::default(),
                    ClosureTable::default(),
                    CallTable::default(),
                    crate::regexp::RegExpTable::default(),
                ),
                FeedbackTableLayout::new(vec![
                    FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(2), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(3), FeedbackKind::Call),
                ]),
                DeoptTable::default(),
                ExceptionTable::default(),
                SourceMap::default(),
            ),
        );
        let module = Module::new(
            Some("inherited-accessor-set"),
            vec![entry],
            FunctionIndex(0),
        )
        .expect("module should be valid");
        let mut runtime = RuntimeState::new();
        let prototype = runtime.alloc_object();
        let object = runtime.alloc_object_with_prototype(Some(prototype));
        let property = runtime.intern_property_name("value");
        let getter = runtime.register_native_function(NativeFunctionDescriptor::getter(
            "value",
            inherited_accessor_getter,
        ));
        let setter = runtime.register_native_function(NativeFunctionDescriptor::setter(
            "value",
            inherited_accessor_setter,
        ));
        let getter = runtime.alloc_host_function(getter);
        let setter = runtime.alloc_host_function(setter);
        runtime
            .objects_mut()
            .define_accessor(prototype, property, Some(getter), Some(setter))
            .expect("prototype accessor should install");
        let registers = [RegisterValue::from_object_handle(object.0)];

        let result = Interpreter::new().execute_with_runtime(
            &module,
            FunctionIndex(0),
            &registers,
            &mut runtime,
        );

        assert_eq!(
            result.map(ExecutionResult::return_value),
            Ok(RegisterValue::from_i32(7))
        );
        let lookup = runtime
            .objects()
            .get_property(object, property)
            .expect("receiver accessor lookup should succeed")
            .expect("receiver accessor should resolve");
        assert_eq!(lookup.owner(), prototype);
        let backing = runtime.intern_property_name("__backing");
        let backing_lookup = runtime
            .objects()
            .get_property(object, backing)
            .expect("receiver backing lookup should succeed")
            .expect("setter should have created receiver backing slot");
        assert_eq!(backing_lookup.owner(), object);
        assert_eq!(
            backing_lookup.value(),
            PropertyValue::data(RegisterValue::from_i32(7))
        );
    }

    #[test]
    fn interpreter_constructs_host_function_with_return_override_rules() {
        let layout = FrameLayout::new(0, 0, 0, 9).expect("frame layout should be valid");
        let entry = Function::new(
            Some("entry"),
            layout,
            Bytecode::from(vec![
                Instruction::call_closure(
                    BytecodeRegister::new(2),
                    BytecodeRegister::new(0),
                    BytecodeRegister::new(8),
                ),
                Instruction::get_property(
                    BytecodeRegister::new(3),
                    BytecodeRegister::new(2),
                    crate::property::PropertyNameId(0),
                ),
                Instruction::call_closure(
                    BytecodeRegister::new(4),
                    BytecodeRegister::new(1),
                    BytecodeRegister::new(8),
                ),
                Instruction::get_property(
                    BytecodeRegister::new(5),
                    BytecodeRegister::new(4),
                    crate::property::PropertyNameId(0),
                ),
                Instruction::load_i32(BytecodeRegister::new(6), 7),
                Instruction::eq(
                    BytecodeRegister::new(6),
                    BytecodeRegister::new(3),
                    BytecodeRegister::new(6),
                ),
                Instruction::jump_if_false(BytecodeRegister::new(6), JumpOffset::new(4)),
                Instruction::load_i32(BytecodeRegister::new(7), 9),
                Instruction::eq(
                    BytecodeRegister::new(7),
                    BytecodeRegister::new(5),
                    BytecodeRegister::new(7),
                ),
                Instruction::ret(BytecodeRegister::new(7)),
                Instruction::load_false(BytecodeRegister::new(7)),
                Instruction::ret(BytecodeRegister::new(7)),
            ]),
            FunctionTables::new(
                FunctionSideTables::new(
                    PropertyNameTable::new(vec!["value"]),
                    StringTable::default(),
                    FloatTable::default(),
                    BigIntTable::default(),
                    ClosureTable::default(),
                    CallTable::new(vec![
                        Some(CallSite::Closure(ClosureCall::new(
                            0,
                            FrameFlags::new(true, true, false),
                        ))),
                        None,
                        Some(CallSite::Closure(ClosureCall::new(
                            0,
                            FrameFlags::new(true, true, false),
                        ))),
                        None,
                        None,
                        None,
                        Some(CallSite::Closure(ClosureCall::new(
                            0,
                            FrameFlags::new(false, true, false),
                        ))),
                        None,
                        None,
                        None,
                        None,
                        None,
                    ]),
                    crate::regexp::RegExpTable::default(),
                ),
                FeedbackTableLayout::new(vec![
                    FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(2), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(3), FeedbackKind::Property),
                    FeedbackSlotLayout::new(FeedbackSlotId(4), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(5), FeedbackKind::Comparison),
                    FeedbackSlotLayout::new(FeedbackSlotId(6), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(7), FeedbackKind::Comparison),
                ]),
                DeoptTable::default(),
                ExceptionTable::default(),
                SourceMap::default(),
            ),
        );
        let module = Module::new(Some("host-construct"), vec![entry], FunctionIndex(0))
            .expect("module should be valid");
        let mut runtime = RuntimeState::new();

        let primitive_constructor =
            runtime.register_native_function(NativeFunctionDescriptor::constructor(
                "PrimitiveCtor",
                0,
                host_constructor_returns_primitive,
            ));
        let object_constructor = runtime.register_native_function(
            NativeFunctionDescriptor::constructor("ObjectCtor", 0, host_constructor_returns_object),
        );
        let primitive_constructor = runtime.alloc_host_function(primitive_constructor);
        let object_constructor = runtime.alloc_host_function(object_constructor);
        let registers = [
            RegisterValue::from_object_handle(primitive_constructor.0),
            RegisterValue::from_object_handle(object_constructor.0),
        ];

        let result = Interpreter::new().execute_with_runtime(
            &module,
            FunctionIndex(0),
            &registers,
            &mut runtime,
        );

        assert_eq!(
            result.map(ExecutionResult::return_value),
            Ok(RegisterValue::from_bool(true))
        );
    }

    #[test]
    fn interpreter_throws_type_error_on_non_constructible_host_function() {
        let layout = FrameLayout::new(0, 0, 0, 2).expect("frame layout should be valid");
        let entry = Function::new(
            Some("entry"),
            layout,
            Bytecode::from(vec![
                Instruction::call_closure(
                    BytecodeRegister::new(1),
                    BytecodeRegister::new(0),
                    BytecodeRegister::new(1),
                ),
                Instruction::ret(BytecodeRegister::new(1)),
            ]),
            FunctionTables::new(
                FunctionSideTables::new(
                    PropertyNameTable::default(),
                    StringTable::default(),
                    FloatTable::default(),
                    BigIntTable::default(),
                    ClosureTable::default(),
                    CallTable::new(vec![
                        Some(CallSite::Closure(ClosureCall::new(
                            0,
                            FrameFlags::new(true, true, false),
                        ))),
                        None,
                    ]),
                    crate::regexp::RegExpTable::default(),
                ),
                FeedbackTableLayout::new(vec![FeedbackSlotLayout::new(
                    FeedbackSlotId(0),
                    FeedbackKind::Call,
                )]),
                DeoptTable::default(),
                ExceptionTable::default(),
                SourceMap::default(),
            ),
        );
        let module = Module::new(Some("bad-construct"), vec![entry], FunctionIndex(0))
            .expect("module should be valid");
        let mut runtime = RuntimeState::new();
        let method = runtime.register_native_function(NativeFunctionDescriptor::method(
            "plain",
            0,
            host_plain_method,
        ));
        let method = runtime.alloc_host_function(method);
        let registers = [RegisterValue::from_object_handle(method.0)];

        let error = Interpreter::new()
            .execute_with_runtime(&module, FunctionIndex(0), &registers, &mut runtime)
            .expect_err("constructing a plain host method should fail");

        assert!(matches!(error, InterpreterError::UncaughtThrow(_)));
    }

    #[test]
    fn interpreter_ordinary_calls_default_this_to_undefined() {
        let entry_layout = FrameLayout::new(0, 0, 0, 2).expect("frame layout should be valid");
        let helper_layout = FrameLayout::new(1, 0, 0, 1).expect("frame layout should be valid");
        let entry = Function::new(
            Some("entry"),
            entry_layout,
            Bytecode::from(vec![
                Instruction::call_direct(BytecodeRegister::new(0), BytecodeRegister::new(0)),
                Instruction::ret(BytecodeRegister::new(0)),
            ]),
            FunctionTables::new(
                FunctionSideTables::new(
                    PropertyNameTable::default(),
                    StringTable::default(),
                    FloatTable::default(),
                    BigIntTable::default(),
                    ClosureTable::default(),
                    CallTable::new(vec![
                        Some(CallSite::Direct(DirectCall::new(
                            FunctionIndex(1),
                            0,
                            FrameFlags::empty(),
                        ))),
                        None,
                    ]),
                    crate::regexp::RegExpTable::default(),
                ),
                FeedbackTableLayout::new(vec![
                    FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Call),
                ]),
                DeoptTable::default(),
                ExceptionTable::default(),
                SourceMap::default(),
            ),
        );
        let helper = Function::with_bytecode(
            Some("helper"),
            helper_layout,
            Bytecode::from(vec![
                Instruction::load_this(BytecodeRegister::new(0)),
                Instruction::ret(BytecodeRegister::new(0)),
            ]),
        );
        let module = Module::new(Some("ordinary-this"), vec![entry, helper], FunctionIndex(0))
            .expect("module should be valid");

        let result = Interpreter::new().execute(&module);

        assert_eq!(
            result.map(ExecutionResult::return_value),
            Ok(RegisterValue::undefined())
        );
    }

    #[test]
    fn interpreter_method_calls_preserve_receiver_in_hidden_slot() {
        let entry_layout = FrameLayout::new(0, 0, 0, 3).expect("frame layout should be valid");
        let closure_layout = FrameLayout::new(1, 0, 0, 1).expect("frame layout should be valid");
        let entry = Function::new(
            Some("entry"),
            entry_layout,
            Bytecode::from(vec![
                Instruction::new_object(BytecodeRegister::new(0)),
                Instruction::new_closure(BytecodeRegister::new(1), BytecodeRegister::new(0)),
                Instruction::call_closure(
                    BytecodeRegister::new(2),
                    BytecodeRegister::new(1),
                    BytecodeRegister::new(0),
                ),
                Instruction::ret(BytecodeRegister::new(2)),
            ]),
            FunctionTables::new(
                FunctionSideTables::new(
                    PropertyNameTable::default(),
                    StringTable::default(),
                    FloatTable::default(),
                    BigIntTable::default(),
                    ClosureTable::new(vec![
                        None,
                        Some(ClosureTemplate::new(FunctionIndex(1), [])),
                        None,
                        None,
                    ]),
                    CallTable::new(vec![
                        None,
                        None,
                        Some(CallSite::Closure(ClosureCall::new_with_receiver(
                            0,
                            FrameFlags::new(false, true, false),
                            BytecodeRegister::new(0),
                        ))),
                        None,
                    ]),
                    crate::regexp::RegExpTable::default(),
                ),
                FeedbackTableLayout::new(vec![
                    FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(2), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(3), FeedbackKind::Call),
                ]),
                DeoptTable::default(),
                ExceptionTable::default(),
                SourceMap::default(),
            ),
        );
        let closure = Function::with_bytecode(
            Some("closure"),
            closure_layout,
            Bytecode::from(vec![
                Instruction::load_this(BytecodeRegister::new(0)),
                Instruction::ret(BytecodeRegister::new(0)),
            ]),
        );
        let module = Module::new(Some("method-this"), vec![entry, closure], FunctionIndex(0))
            .expect("module should be valid");

        let result = Interpreter::new().execute(&module);

        let value = result.expect("method call should execute").return_value();
        assert!(
            value.as_object_handle().is_some(),
            "expected object receiver"
        );
    }

    #[test]
    fn interpreter_host_method_calls_preserve_symbol_primitive_receiver() {
        let layout = FrameLayout::new(0, 0, 0, 3).expect("frame layout should be valid");
        let entry = Function::new(
            Some("entry"),
            layout,
            Bytecode::from(vec![
                Instruction::call_closure(
                    BytecodeRegister::new(2),
                    BytecodeRegister::new(0),
                    BytecodeRegister::new(2),
                ),
                Instruction::ret(BytecodeRegister::new(2)),
            ]),
            FunctionTables::new(
                FunctionSideTables::new(
                    PropertyNameTable::default(),
                    StringTable::default(),
                    FloatTable::default(),
                    BigIntTable::default(),
                    ClosureTable::default(),
                    CallTable::new(vec![
                        Some(CallSite::Closure(ClosureCall::new_with_receiver(
                            0,
                            FrameFlags::new(false, true, false),
                            BytecodeRegister::new(1),
                        ))),
                        None,
                    ]),
                    crate::regexp::RegExpTable::default(),
                ),
                FeedbackTableLayout::new(vec![FeedbackSlotLayout::new(
                    FeedbackSlotId(0),
                    FeedbackKind::Call,
                )]),
                DeoptTable::default(),
                ExceptionTable::default(),
                SourceMap::default(),
            ),
        );
        let module = Module::new(Some("symbol-host-this"), vec![entry], FunctionIndex(0))
            .expect("module should be valid");
        let mut runtime = RuntimeState::new();
        let method = runtime.register_native_function(NativeFunctionDescriptor::method(
            "echoReceiver",
            0,
            host_echo_receiver,
        ));
        let method = runtime.alloc_host_function(method);
        let receiver = runtime
            .intrinsics()
            .well_known_symbol_value(WellKnownSymbol::ToPrimitive);
        let registers = [
            RegisterValue::from_object_handle(method.0),
            receiver,
            RegisterValue::undefined(),
        ];

        let result = Interpreter::new()
            .execute_with_runtime(&module, FunctionIndex(0), &registers, &mut runtime)
            .expect("host symbol receiver call should execute");

        assert_eq!(result.return_value(), receiver);
    }

    #[test]
    fn prepare_direct_call_preserves_construct_flag_and_receiver() {
        let entry_layout = FrameLayout::new(0, 0, 0, 1).expect("frame layout should be valid");
        let helper_layout = FrameLayout::new(1, 0, 0, 0).expect("frame layout should be valid");
        let entry = Function::with_bytecode(Some("entry"), entry_layout, Bytecode::default());
        let helper = Function::with_bytecode(Some("helper"), helper_layout, Bytecode::default());
        let module = Module::new(Some("construct"), vec![entry, helper], FunctionIndex(0))
            .expect("module should be valid");
        let caller_function = module.function(FunctionIndex(0)).expect("entry must exist");
        let callee_function = module
            .function(FunctionIndex(1))
            .expect("helper must exist");
        let mut caller_activation = Activation::new(
            FunctionIndex(0),
            caller_function.frame_layout().register_count(),
        );
        let mut runtime = RuntimeState::new();
        let receiver = runtime.objects.alloc_object();
        caller_activation
            .write_bytecode_register(
                caller_function,
                BytecodeRegister::new(0).index(),
                RegisterValue::from_object_handle(receiver.0),
            )
            .expect("caller receiver register should exist");

        let callee_activation = Interpreter::prepare_direct_call(
            &module,
            caller_function,
            &caller_activation,
            0,
            DirectCall::new_with_receiver(
                FunctionIndex(1),
                0,
                FrameFlags::new(true, true, false),
                BytecodeRegister::new(0),
            ),
        )
        .expect("direct call setup should succeed");

        assert!(callee_activation.metadata().flags().is_construct());
        assert!(callee_activation.metadata().flags().has_receiver());
        assert_eq!(
            callee_activation
                .receiver(callee_function)
                .expect("callee receiver must exist"),
            RegisterValue::from_object_handle(receiver.0)
        );
    }

    #[test]
    fn interpreter_executes_closure_with_upvalue_updates() {
        let entry_layout = FrameLayout::new(0, 0, 0, 6).expect("frame layout should be valid");
        let closure_layout = FrameLayout::new(0, 1, 0, 4).expect("frame layout should be valid");
        let entry = Function::new(
            Some("entry"),
            entry_layout,
            Bytecode::from(vec![
                Instruction::load_i32(BytecodeRegister::new(0), 1),
                Instruction::new_closure(BytecodeRegister::new(1), BytecodeRegister::new(0)),
                Instruction::load_i32(BytecodeRegister::new(2), 41),
                Instruction::call_closure(
                    BytecodeRegister::new(3),
                    BytecodeRegister::new(1),
                    BytecodeRegister::new(2),
                ),
                Instruction::load_i32(BytecodeRegister::new(4), 1),
                Instruction::call_closure(
                    BytecodeRegister::new(5),
                    BytecodeRegister::new(1),
                    BytecodeRegister::new(4),
                ),
                Instruction::ret(BytecodeRegister::new(5)),
            ]),
            FunctionTables::new(
                FunctionSideTables::new(
                    PropertyNameTable::default(),
                    StringTable::default(),
                    FloatTable::default(),
                    BigIntTable::default(),
                    ClosureTable::new(vec![
                        None,
                        Some(ClosureTemplate::new(
                            FunctionIndex(1),
                            [CaptureDescriptor::Register(BytecodeRegister::new(0))],
                        )),
                        None,
                        None,
                        None,
                        None,
                        None,
                    ]),
                    CallTable::new(vec![
                        None,
                        None,
                        None,
                        Some(CallSite::Closure(ClosureCall::new(1, FrameFlags::empty()))),
                        None,
                        Some(CallSite::Closure(ClosureCall::new(1, FrameFlags::empty()))),
                        None,
                    ]),
                    crate::regexp::RegExpTable::default(),
                ),
                FeedbackTableLayout::new(vec![
                    FeedbackSlotLayout::new(FeedbackSlotId(0), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(1), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(2), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(3), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(4), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(5), FeedbackKind::Call),
                    FeedbackSlotLayout::new(FeedbackSlotId(6), FeedbackKind::Call),
                ]),
                DeoptTable::default(),
                ExceptionTable::default(),
                SourceMap::default(),
            ),
        );
        let closure = Function::with_bytecode(
            Some("closure"),
            closure_layout,
            Bytecode::from(vec![
                Instruction::get_upvalue(BytecodeRegister::new(1), UpvalueId(0)),
                Instruction::add(
                    BytecodeRegister::new(2),
                    BytecodeRegister::new(1),
                    BytecodeRegister::new(0),
                ),
                Instruction::set_upvalue(BytecodeRegister::new(2), UpvalueId(0)),
                Instruction::get_upvalue(BytecodeRegister::new(3), UpvalueId(0)),
                Instruction::ret(BytecodeRegister::new(3)),
            ]),
        );
        let module = Module::new(Some("closure"), vec![entry, closure], FunctionIndex(0))
            .expect("module should be valid");

        let result = Interpreter::new().execute(&module);

        assert_eq!(
            result.map(ExecutionResult::return_value),
            Ok(RegisterValue::from_i32(43))
        );
    }
