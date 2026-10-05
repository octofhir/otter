//! Terminal sampling over the canonical construction receiver and ticket.
//!
//! # Contents
//! - `sample_terminal_receiver` reads the collector-updated original receiver.
//! - Canonical frame ticket take/transfer; no instance lives on a layout body.
//!
//! # Invariants
//! The caller invokes terminal sampling before unlinking the frame on normal
//! return, primitive/explicit-object return, or abrupt unwind. A successful super transfers
//! the exact ticket and original receiver to the outer construction owner;
//! ordinary nested `new` owns a separate ticket. An abrupt super samples its
//! own allocated receiver locally; it never transfers a failed installation. No property/prototype lookup,
//! allocation, JavaScript work, or result substitution occurs here. The native
//! terminal validates every nonempty ticket's current frame source owner through
//! the existing borrowed/owned resolution before sampling or Super attribution.
//!
//! # See also
//! - `crate::native_abi::Frame` for the sole physical root fields.
//! - `crate::interp::frames` for normal completion and unwind ownership.

use super::ConstructorLayout;
use crate::{Interpreter, Value};

pub(crate) fn sample_terminal_receiver(
    heap: &mut otter_gc::GcHeap,
    layout: ConstructorLayout,
    receiver: Value,
) -> bool {
    if layout.is_null() {
        return false;
    }
    let Some(receiver) = receiver.as_object() else {
        return false;
    };
    let count = heap.read_payload(receiver, crate::object::ObjectBody::slot_count);
    heap.with_payload(layout, |body| body.record_terminal(count))
}

impl Interpreter {
    /// A family that finished sampling takes no further terminal sample. Once
    /// the receiver's source feedback is recorded, drop the ticket so the
    /// construction completes without a terminal call.
    pub(crate) fn release_sampled_construct_ticket(&self, frame: &mut crate::native_abi::Frame) {
        let layout = frame.construct_layout;
        if !layout.is_null()
            && self
                .gc_heap
                .read_payload(layout, super::ConstructorLayoutBody::samples_remaining)
                == 0
        {
            frame.construct_layout = ConstructorLayout::null();
        }
    }

    /// Consume exactly once while the canonical frame/root fields still live.
    /// Failed derived entry with no receiver has no sample. Abrupt completion
    /// otherwise follows this same operation before frame unlink.
    pub(crate) fn complete_constructor_layout(&mut self, frame: &mut crate::native_abi::Frame) {
        frame.super_origin = 0;
        let layout = std::mem::replace(&mut frame.construct_layout, ConstructorLayout::null());
        let receiver = std::mem::replace(&mut frame.construct_receiver, Value::UNDEFINED);
        if !layout.is_null() {
            let _ = sample_terminal_receiver(&mut self.gc_heap, layout, receiver);
        }
    }
    /// Transfer a `super` ticket, preserving the allocating base family even
    /// if its returned object differs from its original allocated receiver.
    /// The dispatch owner proves this edge is `super`, never an ordinary `new`.
    pub(crate) fn transfer_super_constructor_layout(
        &mut self,
        source: &mut crate::native_abi::Frame,
        outer: &mut crate::native_abi::Frame,
    ) {
        if source.construct_layout.is_null() {
            return;
        }
        if !outer.construct_layout.is_null() || !outer.this_value.is_hole() {
            // An earlier successful Native/Proxy super can bind this without
            // a bytecode layout ticket. The exact caller Super source proves
            // this edge; the canonical binding slot additionally proves that
            // this child cannot install its receiver. Do not delay its sample
            // until the outer terminal, where escaped receivers may have grown.
            // The first successful bytecode super may also own an outer ticket.
            // A subsequent allocation still teaches exactly once locally, even
            // when InitializeThisBinding will reject the second successful call.
            self.complete_constructor_layout(source);
            return;
        }
        // Exact context-slot absence was proved by terminal dispatch for lexical callers.
        outer.construct_layout =
            std::mem::replace(&mut source.construct_layout, ConstructorLayout::null());
        outer.construct_receiver =
            std::mem::replace(&mut source.construct_receiver, Value::UNDEFINED);
    }
}

impl Interpreter {
    /// Caller source owns the super edge; successful construction is resolved
    /// before this call. Derived return validation may have changed Success to
    /// Throw, and any explicit replacement object is already the result.
    /// No source borrow survives an allocation: this entire helper cannot GC.
    pub(crate) fn finish_constructor_layout_terminal(
        &mut self,
        context: &crate::ExecutionContext,
        frame: &mut crate::native_abi::Frame,
        successful: bool,
    ) -> Result<(), crate::VmError> {
        // The source address exists only during this synchronous construct.
        // Consume it before any terminal path; SideExit never enters this helper.
        let origin = std::mem::replace(&mut frame.super_origin, 0);
        if frame.construct_layout.is_null() {
            return Ok(());
        }
        if successful && origin != 0 {
            let mut candidate = frame.caller_frame();
            let mut source_pointer = std::ptr::null_mut();
            // Compare the origin as an address BEFORE dereferencing it. Only
            // the already published caller chain is traversed by this leaf.
            while !candidate.is_null() {
                if candidate as usize as u64 == origin {
                    source_pointer = candidate;
                    break;
                }
                // SAFETY: completing-child terminal retains every synchronous
                // caller record until it returns, without GC, park or reentry.
                candidate = unsafe { (*candidate).caller_frame() };
            }
            if source_pointer.is_null() {
                self.complete_constructor_layout(frame);
                return Err(crate::VmError::InvalidOperand);
            }
            // SAFETY: exact address membership above proved this live source.
            // The chain and source stay published until this noallocation leaf
            // finishes; origin itself is never dereferenced speculatively.
            let caller = unsafe { &*source_pointer };
            if caller.header.kind == crate::native_abi::NativeFrameKind::Host {
                self.complete_constructor_layout(frame);
                return Err(crate::VmError::InvalidOperand);
            }
            let selected = (|| -> Result<Option<*mut crate::native_abi::Frame>, crate::VmError> {
                let (fid, pc) = crate::runtime_activation::semantic_source::frame_semantic_source(
                    self, context, caller,
                )?;
                let source = context
                    .for_function(fid)
                    .map_err(|_| crate::VmError::InvalidOperand)?;
                let function = source
                    .exec_function(fid)
                    .ok_or(crate::VmError::InvalidOperand)?;
                let instruction = function
                    .instr_at_index(pc as usize)
                    .ok_or(crate::VmError::InvalidOperand)?;
                if !matches!(
                    function.op(instruction),
                    otter_bytecode::Op::SuperConstruct | otter_bytecode::Op::SuperConstructSpread
                ) {
                    return Err(crate::VmError::InvalidOperand);
                }
                super::lexical::owner(self, context, source_pointer)
            })();
            let owner = match selected {
                Ok(owner) => owner,
                Err(error) => {
                    self.complete_constructor_layout(frame);
                    return Err(error);
                }
            };
            if let Some(owner) = owner {
                // SAFETY: exact own-context identity proves this live ancestor,
                // distinct from the completing child; immutable borrows ended.
                let outer = unsafe { &mut *owner };
                let unbound = match super::lexical::this_is_unbound(self, context, outer) {
                    Ok(unbound) => unbound,
                    Err(error) => {
                        self.complete_constructor_layout(frame);
                        return Err(error);
                    }
                };
                if unbound {
                    self.transfer_super_constructor_layout(frame, outer);
                } else {
                    self.complete_constructor_layout(frame);
                }
                return Ok(());
            }
        }
        // Abrupt super never installs this in the caller. Ordinary nested new
        // similarly owns its own terminal even within a construct activation.
        self.complete_constructor_layout(frame);
        Ok(())
    }
}

/// Private engine leaf after resolved constructor return semantics and before
/// unlink. The caller parks the sole pair in an aligned 16-byte native slot;
/// that slot and ctx/current frame remain initialized for this whole call.
/// Canonical original-receiver/layout fields are traced until completion. The
/// entry neither prepares final shapes nor allocates, reenters, or collects.
/// SideExit preserves the ticket for canonical in-place deopt continuation.
///
/// # Safety
/// `pair` points at one initialized NativeResultPair retained for this call;
/// `ctx` and its published frame/services satisfy the compiled entry lifetime.
pub unsafe extern "C" fn constructor_terminal(
    ctx: *mut crate::native_abi::JitCtx,
    pair: *const crate::native_abi::NativeResultPair,
) -> crate::native_abi::NativeResultPair {
    use crate::native_abi::{NativeResultDomain, NativeResultPair, NativeResultStatus};
    // SAFETY: both pointer lifetimes are owned by the private entry contract.
    let Some(pair) = (unsafe { pair.as_ref() }).copied() else {
        return NativeResultPair::fatal_internal();
    };
    let status = pair.validate(NativeResultDomain::Compiled);
    if status == Some(NativeResultStatus::SideExit) {
        return pair;
    }
    if !matches!(
        status,
        Some(NativeResultStatus::Success | NativeResultStatus::Throw | NativeResultStatus::Fatal)
    ) {
        return NativeResultPair::fatal_internal();
    }
    let Some(ctx) = (unsafe { ctx.as_mut() }) else {
        return NativeResultPair::fatal_internal();
    };
    let Some(frame) = (unsafe { ctx.native_frame.as_mut() }) else {
        return NativeResultPair::fatal_internal();
    };
    if frame.construct_layout.is_null() {
        frame.super_origin = 0;
        return pair;
    }
    let Some(activation) = ctx.checked_activation().copied() else {
        return NativeResultPair::fatal_internal();
    };
    let Some(vm) = (unsafe { activation.vm.as_mut() }) else {
        return NativeResultPair::fatal_internal();
    };
    // Validate the actual frame owner even when its terminal only samples
    // locally. Borrow the admitted current-space chunk when it covers this
    // FID; foreign FIDs retain the existing owned resolution. Native-only or
    // unrelated-space admission resolves through this VM's actual CodeSpace.
    // No owned copy of an already resolved context is needed by this leaf.
    let context = match unsafe { activation.context.as_ref() } {
        Some(context) if std::sync::Arc::ptr_eq(context.space(), &vm.code_space) => context
            .for_function(frame.header.function_id)
            .map_err(|_| crate::VmError::InvalidOperand),
        _ => vm
            .function_context(None, frame.header.function_id)
            .map(crate::code_space::ResolvedCtx::Owned),
    };
    let Ok(context) = context else {
        return NativeResultPair::fatal_internal();
    };
    if let Err(error) = vm.finish_constructor_layout_terminal(
        &context,
        frame,
        status == Some(NativeResultStatus::Success),
    ) {
        // This is malformed source metadata, never deferred finalization OOM.
        // Use the one current engine fatal owner; no JS error is constructed.
        if let Some(slot) = unsafe { ctx.error.as_mut() } {
            *slot = Some(error);
        }
        return NativeResultPair::fatal_internal();
    }
    pair
}

impl Interpreter {
    pub(crate) fn complete_unentered_constructor_layout(
        &mut self,
        request: &mut crate::native_abi::CallRequest,
    ) {
        request.super_origin = 0;
        let layout = std::mem::replace(&mut request.construct_layout, ConstructorLayout::null());
        let receiver = std::mem::replace(&mut request.construct_receiver, Value::UNDEFINED);
        if !layout.is_null() {
            let _ = sample_terminal_receiver(&mut self.gc_heap, layout, receiver);
        }
    }
}
