//! Implicit error ownership across inlined chunks in one source realm.
//!
//! # Contents
//! - Distinct physical and logical kind constants at one published safepoint.
//! - Exact logical error-class prototype across moving collection.
//!
//! # Invariants
//! Metadata-only code is never executed. The real committed scalar owner
//! allocates the error from the existing published activation and source map.
//! Both chunks share a realm, as required by intrinsic inline admission.
//! Rooted result and source prototype are re-read after collection.
//!
//! # See also
//! - `super::semantic_source` owns logical descendant identity.
//! - `crate::error_ops` owns source-realm intrinsic error allocation.

use super::RuntimeCall;
use crate::jit::{JitFunctionCode, VmRuntimeActivation};
use crate::native_abi::{
    CodeObjectMetadata, Frame, NativeFrameFlags, NativeFrameKind, SafepointRecord, SpillRoots,
    VmFrameHeader,
};
use crate::{ErrorKind, Interpreter, Value};
use otter_bytecode::{Constant, FunctionCodeBuilder, Op, Operand};
use std::{ptr::NonNull, sync::Arc};

#[derive(Debug)]
struct SourceCode {
    function_id: u32,
    record: SafepointRecord,
}

impl JitFunctionCode for SourceCode {
    fn metadata(&self) -> CodeObjectMetadata {
        CodeObjectMetadata {
            id: 91,
            code_block_id: self.function_id,
            entry_offset: 0,
            code_size: 1,
            safepoint_count: 1,
            frame_map_count: 0,
            spill_map_count: 0,
            dependency_count: 0,
        }
    }

    fn native_frame_kind(&self) -> NativeFrameKind {
        NativeFrameKind::Optimizing
    }

    fn code_len(&self) -> usize {
        1
    }

    fn safepoint_record(&self, id: u32) -> Option<&SafepointRecord> {
        (id == self.record.id).then_some(&self.record)
    }
}

fn error_module(name: &str, kind: &str) -> crate::BytecodeModule {
    let mut module = crate::test_support::minimal_bytecode_module(name);
    module.constants.push(Constant::String {
        utf16: kind.encode_utf16().collect(),
    });
    module.functions[0].locals = 2;
    let mut code = FunctionCodeBuilder::new();
    code.push(
        Op::NewError,
        &[
            Operand::Register(0),
            Operand::ConstIndex(0),
            Operand::Register(1),
        ],
    );
    code.push(Op::Return, &[Operand::Register(0)]);
    module.functions[0].code = code.finish();
    module
}

#[test]
fn inline_error_uses_its_source_kind_across_chunks() {
    let mut vm = Interpreter::new().expect("fixture interpreter bootstrap");
    let outer = vm
        .link_module(
            error_module("outer-error-source.js", "TypeError"),
            crate::source_registry::SourceRegistry::default(),
        )
        .expect("physical source chunk");
    let inner = vm
        .link_module(
            error_module("inner-error-source.js", "RangeError"),
            crate::source_registry::SourceRegistry::default(),
        )
        .expect("logical source chunk");
    let source_prototype = vm.persistent_root_insert(Value::object(
        vm.error_classes.prototype(ErrorKind::RangeError),
    ));
    let outer_id = outer.main().id;
    let inner_id = inner.main().id;
    assert_ne!(outer_id, inner_id);
    assert_eq!(
        vm.function_realm_id(outer_id),
        vm.function_realm_id(inner_id)
    );
    assert_eq!(
        outer.string_constant_str_for_function(outer_id, 0),
        Some("TypeError")
    );
    assert!(
        outer
            .string_constant_str_for_function(inner_id, 0)
            .is_none()
    );
    {
        let source = outer
            .for_function(inner_id)
            .expect("logical sibling source owner");
        assert_eq!(
            source.string_constant_str_for_function(inner_id, 0),
            Some("RangeError")
        );
    }
    let record = SafepointRecord {
        id: 17,
        frame_state: crate::native_abi::NO_FRAME_STATE,
        spill_roots: SpillRoots::from_slots([]),
        inline_frames: Box::new([crate::deopt::DeoptFrame {
            function_id: inner_id,
            byte_pc: 0,
            entry: None,
            register_count: 2,
            slots: Box::new([]),
            arguments: None,
        }]),
        call_pc: 0,
    };
    vm.jit_code_registry
        .register(
            91,
            Arc::new(SourceCode {
                function_id: outer_id,
                record,
            }),
        )
        .expect("retained source recipe");
    let mut stack = crate::ActivationStack::new();
    vm.with_runtime_turn(&mut stack, |turn| {
        let (vm, stack) = turn.into_parts();
        let mut registers = [Value::undefined(); 2];
        let mut frame = Frame::new(
            VmFrameHeader {
                function_id: outer_id,
                pc: 0,
                register_count: 2,
                kind: NativeFrameKind::Optimizing,
                flags: NativeFrameFlags::from_bits(NativeFrameFlags::HAS_SAFEPOINTS),
            },
            registers.as_mut_ptr() as u64,
            Value::function(outer_id),
            Value::undefined(),
        );
        frame.code_object_id = 91;
        frame.call_site = 17;
        // SAFETY: the frame and initialized window remain stable through pop.
        unsafe { vm.jit_push_native_frame(&mut frame) }.expect("published frame");
        vm.gc_heap.set_gc_stress(1, false);
        let mut activation = VmRuntimeActivation::new(vm, stack, Some(&outer));
        {
            // SAFETY: this activation exclusively owns the published frame.
            let mut call = unsafe {
                RuntimeCall::bind(NonNull::from(&mut activation), NonNull::from(&mut frame))
            }
            .expect("inline scalar activation");
            assert_eq!(call.function_id(), outer_id);
            assert_eq!(call.semantic_source().unwrap().0, inner_id);
            let error = call
                .scalar_values(Value::undefined(), Value::undefined())
                .expect("source-owned intrinsic error");
            call.write(0, error).expect("root returned error");
        }
        vm.force_gc().expect("move returned error and source roots");
        let prototype = crate::object::prototype(registers[0].as_object().unwrap(), &vm.gc_heap);
        assert_eq!(
            prototype,
            vm.persistent_root_get(source_prototype)
                .unwrap()
                .as_object()
        );
        assert_ne!(
            prototype,
            Some(vm.error_classes.prototype(ErrorKind::TypeError))
        );
        assert_eq!(vm.active_realm_id, 0);
        assert_eq!(frame.header.pc, 0);
        vm.jit_pop_native_frame();
    });
    vm.persistent_root_remove(source_prototype).unwrap();
}
