//! Shared execution context and callable entry ABI for the JavaScript stack.
//!
//! # Contents
//! - [`JitCtx`] carries isolate services and the currently published frame.
//! - [`JitEntry`] is the entry signature used by every execution tier.
//! - Checked runtime-call binding and operation-scoped frame access.
//!
//! # Invariants
//! The VM owns this machine-visible layout. Interpreter and generated entries
//! receive the same context; JavaScript state lives in its published frame.
//! Frame and service pointers remain valid throughout the entry extent. A
//! runtime operation never retains a slot borrow across allocation or reentry.
//!
//! # See also
//! - [`super::Frame`] for activation state and caller links.
//! - [`super::RuntimeCall`] for the typed semantic boundary.

use super::{
    ActiveFrameMut, ActiveFrameRef, CallRequest, Frame, NativeResultPair, RuntimeCall, VmThread,
};
use crate::{Value, VmError, VmRuntimeActivation};

/// Machine-visible context shared by every compiled tier.
///
/// The context contains execution services, not duplicated JavaScript frame
/// state. Registers, SELF, `this`, PC, and tier state live only in
/// [`Frame`]; every compiled tier resolves them through that canonical
/// activation, and captured bindings through the contexts its registers hold.
/// Nested calls reuse this context: linkage links the callee's frame to the
/// current one and makes it `native_frame` for the dynamic extent of the
/// call. An optimized frame names its in-progress call's safepoint in its
/// record, which locates its allocator-owned tagged root homes.
#[repr(C)]
pub struct JitCtx {
    /// Sole machine-visible VM state pointer.
    pub thread: *mut VmThread,
    /// Innermost published frame: the frame now executing generated code.
    /// The VM reads the frame chain through this cell.
    pub native_frame: *mut Frame,
    /// Error slot shared by direct callees and runtime stubs when a re-entered
    /// operation throws. Pointer form keeps the slot stable while the shared
    /// context swaps its active native frame for a nested callee.
    pub error: *mut Option<VmError>,
    /// A generated call links a new frame only while the caller's
    /// [`Frame::depth`] is below this bound, which keeps interpreter
    /// plus generated frames within the JavaScript depth budget.
    pub generated_depth_limit: u64,
    /// Address of the active realm's GC-rooted `globalThis` compressed offset.
    pub global_this_offset: *const u32,
    /// Lowest native-stack address generated callees may reserve.
    pub native_stack_limit: usize,
    /// One while no generated entry has occurred in this outer activation;
    /// generated linkage clears it with one idempotent store. Exact aggregate
    /// counts come from per-generation feedback during cold reconciliation.
    pub generated_feedback_clean: u64,
    /// Audited nursery page and per-type accounting pointers for inline
    /// receiver allocation. A null page forces the rooted cold boundary.
    pub alloc_window: crate::jit::JitMachineAllocationWindow,
    /// Stable aggregate counter record updated by generated allocation paths.
    pub runtime_stats: *mut crate::JitRuntimeStats,
    /// Request consumed by the trampoline after an execution continuation.
    pub pending_call: CallRequest,
    /// Child completion consumed by the resumed entry before any safepoint.
    pub completion: NativeResultPair,
    /// Caller register, MAX for native ABI, or TIER_COMPLETION_DESTINATION.
    pub completion_destination: u32,
    /// Exact generation returned by a tier transfer, including cold inline deopt.
    pub completion_generation: u32,
}

impl JitCtx {
    /// Bind the current machine-published frame to the VM-owned typed runtime
    /// boundary. This is the sole unsafe reconstruction point used by semantic
    /// stubs; [`RuntimeCall`] exposes no raw VM, stack, context, or frame handles.
    pub fn runtime_call(&mut self) -> Result<RuntimeCall<'_>, VmError> {
        let thread = unsafe { self.thread.as_ref() }.ok_or(VmError::InvalidOperand)?;
        let runtime_context = thread.runtime_context;
        if runtime_context == 0 {
            return Err(VmError::InvalidOperand);
        }
        // SAFETY: the common trampoline publishes this activation and native
        // frame for the dynamic extent of the shared JitCtx. `&mut self`
        // prevents a second RuntimeCall from being bound concurrently.
        let activation = std::ptr::NonNull::new(runtime_context as *mut VmRuntimeActivation)
            .ok_or(VmError::InvalidOperand)?;
        let frame = std::ptr::NonNull::new(self.native_frame).ok_or(VmError::InvalidOperand)?;
        // SAFETY: the shared entry ABI publishes both records and their
        // windows for the complete runtime-stub call.
        unsafe { RuntimeCall::bind(activation, frame) }
    }

    /// Stage `arguments` as the actual span of `pending_call` in the activation's
    /// staging buffer. The span stays valid until the trampoline copies it.
    pub fn stage_call_arguments(
        &mut self,
        arguments: impl IntoIterator<Item = Value>,
    ) -> Result<(), VmError> {
        let activation = self.checked_activation().copied().ok_or(VmError::InvalidOperand)?;
        // SAFETY: the published activation retains its stack for this entry.
        let stack = unsafe { activation.stack.as_mut() }.ok_or(VmError::InvalidOperand)?;
        let (pointer, count) = stack
            .stage_arguments(arguments)
            .ok_or(VmError::InvalidOperand)?;
        self.pending_call.arguments = pointer;
        self.pending_call.argument_count = count;
        Ok(())
    }

    /// Stage a dense spread array's elements as the pending call's actuals.
    pub fn stage_spread_arguments(&mut self, array: Value) -> Result<(), VmError> {
        let activation = self.checked_activation().copied().ok_or(VmError::InvalidOperand)?;
        // SAFETY: the published activation retains its VM for this entry.
        let vm = unsafe { activation.vm.as_ref() }.ok_or(VmError::InvalidOperand)?;
        let array = array.as_array().ok_or(VmError::TypeMismatch)?;
        let elements: smallvec::SmallVec<[Value; 8]> =
            crate::array::with_elements(array, &vm.gc_heap, |values| values.iter().copied().collect());
        self.stage_call_arguments(elements)
    }

    /// Write a complete ordinary `[[Call]]` request with staged actuals.
    pub fn stage_call_request(
        &mut self,
        callee: Value,
        receiver: Value,
        arguments: impl IntoIterator<Item = Value>,
    ) -> Result<(), VmError> {
        let mut request = CallRequest::EMPTY;
        request.callee = callee;
        request.receiver = receiver;
        self.pending_call = request;
        self.stage_call_arguments(arguments)
    }

    /// Try the typed boundary for pure-code fixture entries that deliberately
    /// publish no runtime context.
    pub fn try_runtime_call(&mut self) -> Result<Option<RuntimeCall<'_>>, VmError> {
        let thread = unsafe { self.thread.as_ref() }.ok_or(VmError::InvalidOperand)?;
        if thread.runtime_context == 0 {
            return Ok(None);
        }
        self.runtime_call().map(Some)
    }

    /// Representation-neutral shared view of the canonical activation.
    pub fn active_frame(&self) -> Result<ActiveFrameRef<'_>, VmError> {
        // SAFETY: the JIT entry contract publishes this frame and its windows
        // for the complete dynamic extent of `self`. A shared context borrow
        // cannot mutate either descriptor while the view is live.
        unsafe { ActiveFrameRef::from_ptr(self.native_frame) }.map_err(|_| VmError::InvalidOperand)
    }

    /// Representation-neutral mutable view of the canonical activation.
    pub fn active_frame_mut(&mut self) -> Result<ActiveFrameMut<'_>, VmError> {
        // SAFETY: the JIT entry contract publishes this frame for the complete
        // dynamic extent of `self`; `&mut self` provides exclusive logical
        // access to its header and window descriptors. ActiveFrame keeps those
        // windows raw so no slice borrow spans reentrant/allocating VM work.
        unsafe { ActiveFrameMut::from_ptr(self.native_frame) }.map_err(|_| VmError::InvalidOperand)
    }

    /// Stable register-window base derived from the canonical frame.
    pub fn register_base(&mut self) -> Result<*mut Value, VmError> {
        Ok(self.active_frame_mut()?.register_base_ptr())
    }

    /// Absolute index of the currently published activation.
    pub fn frame_index(&self) -> Result<usize, VmError> {
        let frame = unsafe { self.native_frame.as_ref() }.ok_or(VmError::InvalidOperand)?;
        usize::try_from(frame.depth)
            .ok()
            .and_then(|depth| depth.checked_sub(1))
            .ok_or(VmError::InvalidOperand)
    }

    /// VM-owned activation published through the sole machine-visible thread
    /// pointer. Runtime stubs use this explicitly; emitted code never observes
    /// its Rust pointers or container types.
    pub fn activation(&self) -> &VmRuntimeActivation {
        // SAFETY: runtime-capable contexts point at the VmThread built for the
        // current entry, whose runtime_context retains VmRuntimeActivation.
        unsafe { &*((*self.thread).runtime_context as *const VmRuntimeActivation) }
    }

    /// Published activation, or `None` when this entry carries no runtime
    /// context (fixture entries drive pure compiled code with no interpreter).
    /// The cooperative poll boundary must stay sound for such entries instead
    /// of dereferencing an absent activation.
    pub fn checked_activation(&self) -> Option<&VmRuntimeActivation> {
        if self.thread.is_null() {
            return None;
        }
        // SAFETY: a non-null thread points at the VmThread built for the
        // current entry.
        let runtime_context = unsafe { (*self.thread).runtime_context };
        if runtime_context == 0 {
            return None;
        }
        // SAFETY: a nonzero runtime_context retains VmRuntimeActivation for
        // this entry's dynamic extent.
        Some(unsafe { &*(runtime_context as *const VmRuntimeActivation) })
    }
}

/// JavaScript execution entry shared by the interpreter and generated tiers.
pub type JitEntry = extern "C" fn(*mut JitCtx) -> NativeResultPair;

#[cfg(target_pointer_width = "64")]
const _: [(); 192] = [(); std::mem::size_of::<JitCtx>()];

const _: [(); 188] = [(); std::mem::offset_of!(JitCtx, completion_generation)];
