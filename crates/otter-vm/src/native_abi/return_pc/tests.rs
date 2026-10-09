//! Exact table validation and real moving-root consumers of suspended JS sites.
//!
//! # Contents
//! - Wrong-generation, non-site and retired-generation resolution boundaries.
//! - Complete pending request roots before publication, then a physical Host child.
//!
//! # Invariants
//! Synthetic code bytes exercise only metadata lookup, never native execution.
//! The second proof performs actual VM collection with the canonical context and
//! published roots, poisoning old PC/safepoint words to exclude stamp fallback.
//! Assertions stay in ordinary Rust test code, outside any C callback.

use super::*;
use crate::native_abi::{
    self as abi, CallRequest, CodeObjectMetadata, Frame, JitCtx, NativeFrameKind, NativeResultPair,
    SafepointEntry, VmFrameHeader, VmThread,
};
use crate::{ActivationStack, Interpreter, Value};
use std::sync::Arc;

#[derive(Debug)]
struct TableCode {
    bytes: Box<[u8; 64]>,
    records: Box<[SafepointRecord]>,
    sites: Box<[SafepointEntry]>,
}
impl JitFunctionCode for TableCode {
    fn metadata(&self) -> CodeObjectMetadata {
        CodeObjectMetadata {
            id: 91,
            code_block_id: 0,
            entry_offset: 0,
            code_size: 64,
            safepoint_count: self.records.len() as u32,
            frame_map_count: 0,
            spill_map_count: 0,
            dependency_count: 0,
        }
    }
    fn code_len(&self) -> usize {
        64
    }
    fn native_code_address(&self) -> Option<u64> {
        Some(self.bytes.as_ptr() as u64)
    }
    fn return_sites(&self) -> &[SafepointEntry] {
        &self.sites
    }
    fn safepoint_record(&self, id: u32) -> Option<&SafepointRecord> {
        self.records.iter().find(|record| record.id == id)
    }
}
fn code() -> TableCode {
    TableCode {
        bytes: Box::new([0; 64]),
        records: Box::new([SafepointRecord {
            id: 17,
            frame_state: abi::NO_FRAME_STATE,
            spill_roots: abi::SpillRoots::from_slots([0]),
            inline_frames: Box::new([]),
            call_pc: 7,
        }]),
        sites: Box::new([
            SafepointEntry {
                native_return_offset: 12,
                safepoint_id: 17,
            },
            SafepointEntry {
                native_return_offset: 28,
                safepoint_id: 17,
            },
        ]),
    }
}
#[test]
fn exact_return_sites_refuse_wrong_offsets_and_keep_invalid_active_generation_metadata() {
    let mut registry = crate::jit_registry::JitCodeRegistry::new_boxed();
    let code = Arc::new(code());
    let base = code.native_code_address().unwrap();
    registry.register(91, code.clone()).unwrap();
    for offset in [12, 28] {
        assert_eq!(registry.return_pc_record(91, base + offset).unwrap().id, 17);
    }
    for address in [0, base - 1, base, base + 11, base + 13, base + 64, u64::MAX] {
        assert!(
            registry.return_pc_record(91, address).is_none(),
            "{address:x}"
        );
    }
    assert!(registry.return_pc_record(92, base + 12).is_none());
    // SAFETY: the boxed registry retains this stable published view.
    let view = unsafe { *(registry.view_addr() as *const abi::CodeRegistryView) };
    assert!(unsafe { view.resolve_return(91, base + 12) }.is_some());
    assert!(unsafe { view.resolve_return(92, base + 12) }.is_none());
    registry.invalidate_code_object(91);
    assert!(registry.return_pc_record(91, base + 12).is_some());
    assert_eq!(
        registry.retire_unreferenced(),
        0,
        "external activation owner keeps invalid mapping/records"
    );
    drop(code);
    assert_eq!(registry.retire_unreferenced(), 1);
    assert!(registry.return_pc_record(91, base + 12).is_none());
}
#[test]
fn installation_refuses_duplicate_unsorted_out_of_range_and_missing_source_sites() {
    for sites in [vec![12, 12], vec![28, 12], vec![0], vec![64]] {
        let mut code = code();
        code.sites = sites
            .into_iter()
            .map(|native_return_offset| SafepointEntry {
                native_return_offset,
                safepoint_id: 17,
            })
            .collect();
        assert!(!valid_return_sites(&code));
        let mut registry = crate::jit_registry::JitCodeRegistry::new_boxed();
        let available = registry.available_code_bytes();
        assert!(matches!(
            registry.register(91, Arc::new(code)),
            Err(crate::jit_registry::JitInstallError::InvalidCode)
        ));
        assert_eq!(
            registry.available_code_bytes(),
            available,
            "invalid table consumes no executable lease"
        );
    }
    let mut owner = code();
    owner.records[0].call_pc = NO_CALL_PC;
    assert!(!valid_return_sites(&owner));
    let mut registry = crate::jit_registry::JitCodeRegistry::new_boxed();
    assert!(matches!(
        registry.register(91, Arc::new(owner)),
        Err(crate::jit_registry::JitInstallError::InvalidCode)
    ));
    let mut owner = code();
    owner.records[0].call_pc = 7;
    owner.sites[0].safepoint_id = 18;
    assert!(!valid_return_sites(&owner));
    let mut registry = crate::jit_registry::JitCodeRegistry::new_boxed();
    assert!(matches!(
        registry.register(91, Arc::new(owner)),
        Err(crate::jit_registry::JitInstallError::InvalidCode)
    ));
}
#[test]
fn pending_and_published_host_edges_trace_real_moving_homes_without_pc_stamps() {
    let mut vm = Interpreter::new().expect("fixture bootstrap");
    let owner = Arc::new(code());
    let pc = owner.native_code_address().unwrap() + 12;
    vm.jit_code_registry.register(91, owner).unwrap();
    let mut stack = ActivationStack::new();
    vm.with_runtime_turn(&mut stack, |turn| {
        let (vm, _) = turn.into_parts();
        let object = vm.alloc_host_object_with_roots(&[], &[]).unwrap();
        let value = Value::object(object);
        let before = value.to_bits();
        let mut home = [value];
        let mut actuals = [value];
        let mut initial = [value];
        let mut parent = Frame::new(
            VmFrameHeader {
                function_id: 0,
                pc: 999,
                register_count: 0,
                kind: NativeFrameKind::Optimizing,
                flags: abi::NativeFrameFlags::empty(),
            },
            0,
            Value::function(0),
            Value::UNDEFINED,
        );
        parent.code_object_id = 91;
        parent.call_site = 42;
        parent.machine_roots = home.as_mut_ptr() as u64;
        let mut thread = VmThread::empty();
        let mut error = None;
        let mut ctx = JitCtx {
            thread: &mut thread,
            native_frame: &mut parent,
            error: &mut error,
            generated_depth_limit: 8,
            global_this_offset: std::ptr::null(),
            native_stack_limit: 0,
            generated_feedback_clean: 1,
            alloc_window: crate::jit::JitMachineAllocationWindow::disabled(),
            runtime_stats: std::ptr::null_mut(),
            pending_call: CallRequest::EMPTY,
            completion: NativeResultPair::success(Value::UNDEFINED),
            completion_destination: u32::MAX,
            completion_generation: 0,
        };
        thread.frame_cell = std::ptr::from_mut(&mut ctx.native_frame) as u64;
        assert_ne!(thread.frame_cell, 0);
        ctx.pending_call.header = VmFrameHeader::interpreter(0, 1);
        ctx.pending_call.caller = std::ptr::from_ref(&parent) as u64;
        ctx.pending_call.caller_return_pc = pc;
        ctx.pending_call.callee = value;
        ctx.pending_call.receiver = value;
        ctx.pending_call.new_target = value;
        ctx.pending_call.construct_receiver = value;
        ctx.pending_call.arguments = actuals.as_mut_ptr();
        ctx.pending_call.argument_count = 1;
        ctx.pending_call.initial_registers = initial.as_mut_ptr();
        ctx.pending_call.initial_register_count = 1;
        let previous = vm.jit_context.replace(std::ptr::NonNull::from(&mut ctx));
        assert_eq!(
            vm.jit_anchored_safepoint(&parent, vm.jit_frame_return_pc(&parent))
                .unwrap()
                .unwrap()
                .call_pc,
            7
        );
        vm.force_gc().unwrap();
        assert_ne!(home[0].to_bits(), before, "actual nursery evacuation");
        for rooted in [
            actuals[0],
            initial[0],
            ctx.pending_call.callee,
            ctx.pending_call.receiver,
            ctx.pending_call.new_target,
            ctx.pending_call.construct_receiver,
        ] {
            assert_eq!(rooted, home[0]);
        }
        // A Host frame is omitted from source output, but owns this parent's
        // genuine suspended edge. Pending transfer is inactive after publication.
        ctx.pending_call = CallRequest::EMPTY;
        assert_eq!(ctx.pending_call.caller, 0);
        let mut child = Frame::new(
            VmFrameHeader {
                function_id: 0,
                pc: 0,
                register_count: 0,
                kind: NativeFrameKind::Host,
                flags: abi::NativeFrameFlags::empty(),
            },
            0,
            Value::UNDEFINED,
            Value::UNDEFINED,
        );
        child.caller = std::ptr::from_ref(&parent) as u64;
        child.caller_return_pc = pc;
        ctx.native_frame = &mut child;
        assert_eq!(ctx.native_frame, std::ptr::from_mut(&mut child));
        assert_eq!(
            vm.jit_anchored_safepoint(&parent, vm.jit_frame_return_pc(&parent))
                .unwrap()
                .unwrap()
                .call_pc,
            7
        );
        let young = vm.alloc_host_object_with_roots(&[], &[]).unwrap();
        home[0] = Value::object(young);
        let before = home[0].to_bits();
        vm.force_gc().unwrap();
        assert_ne!(home[0].to_bits(), before);
        child.caller_return_pc = pc + 1;
        assert_eq!(child.caller_return_pc, pc + 1);
        assert!(
            matches!(
                vm.jit_anchored_safepoint(&parent, vm.jit_frame_return_pc(&parent)),
                Err(crate::VmError::InvalidOperand)
            ),
            "non-site cannot use poisoned PC/call-site fallback"
        );
        // Cold staging can leave its source-bearing Template window ID after
        // the child returns. A later C helper publishes only the current PC;
        // its semantics/source must not rewind to that previous JS operation.
        ctx.native_frame = &mut parent;
        assert_eq!(ctx.native_frame, std::ptr::from_mut(&mut parent));
        parent.call_site = 17;
        parent.header.kind = NativeFrameKind::Baseline;
        parent.header.pc = 13;
        let record = vm
            .jit_anchored_safepoint(&parent, vm.jit_frame_return_pc(&parent))
            .unwrap();
        assert_eq!(
            record.unwrap().call_pc,
            7,
            "retained source map is intentionally stale"
        );
        assert_eq!(
            vm.jit_frame_return_pc(&parent),
            0,
            "no suspended edge after return"
        );
        assert_eq!(
            vm.jit_frame_source_pc(&parent, vm.jit_frame_return_pc(&parent), record),
            13,
            "current active Template helper source"
        );
        parent.header.kind = NativeFrameKind::Optimizing;
        assert_eq!(
            vm.jit_frame_source_pc(&parent, vm.jit_frame_return_pc(&parent), record),
            7,
            "Graph helper retains its explicit source-bearing map"
        );
        vm.jit_context = previous;
    });
}
