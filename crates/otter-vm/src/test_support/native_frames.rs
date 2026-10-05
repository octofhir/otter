//! Published native frame fixtures for opcode and collector tests.
//!
//! # Contents
//! - Owned tagged buffers and stable frame records for isolated kernels.
//! - A fixture chain exposing the production activation view.
//! - Native continuation execution for already prepared deopt fixtures.
//!
//! # Invariants
//! Buffers and records are pinned by their boxes. Fixtures never participate
//! in production call dispatch: execution tests enter through the trampoline.
//! A fixture chain publishes exactly the same frame cell and root layout as
//! a generated entry, without retaining a VM register arena.
//!
//! # See also
//! - [`crate::native_abi::Frame`]
//! - [`crate::activation_stack::ActivationStack`]

use crate::{ActivationStack, Frame, Interpreter, Value, VmError, native_abi::JitCtx};

/// Drive an already prepared physical fixture through the real trampoline.
/// The enclosing unit-test owner retains its VM, chain and tagged windows.
pub(crate) fn resume_prepared_frame(
    vm: &mut Interpreter,
    stack: &mut ActivationStack,
    context: &crate::ExecutionContext,
    frame: *mut Frame,
) -> Result<Value, VmError> {
    use crate::native_abi::{
        CallRequest, NativeResultDomain, NativeResultPair, NativeResultStatus,
    };
    let ctx = stack.execution_context();
    if ctx.is_null() || unsafe { (*ctx).native_frame } != frame {
        return Err(VmError::InvalidOperand);
    }
    let mut activation = crate::jit::VmRuntimeActivation::new(vm, stack, Some(context));
    let mut thread = crate::native_abi::VmThread::empty();
    thread.runtime_context = std::ptr::from_mut(&mut activation) as u64;
    thread.code_registry = vm.jit_code_registry_view_addr();
    thread.interrupt_cell = vm.jit_interrupt_flag_ptr() as u64;
    thread.gc_heap = vm.jit_gc_heap_ptr() as u64;
    thread.backedge_fuel_cell = vm.jit_backedge_fuel_ptr() as u64;
    thread.global_lexical_epoch_cell = vm.jit_global_lexical_epoch_addr() as u64;
    thread.marking_flag_cell = vm.jit_marking_flag_ptr() as u64;
    thread.array_index_protector_cell = vm.jit_array_index_protector_addr() as u64;
    thread.active_realm_cell = vm.jit_active_realm_addr() as u64;
    thread.array_buffer_detach_protector_cell = vm.jit_array_buffer_detach_protector_addr() as u64;
    thread.frame_cell = unsafe { std::ptr::from_mut(&mut (*ctx).native_frame) as u64 };
    let mut error = None;
    // SAFETY: the fixture's rooted turn retains the published frame and context.
    // These stack services stay live until the assembly continuation returns.
    let (previous_thread, previous_error, previous_window) = unsafe {
        let previous = ((*ctx).thread, (*ctx).error, (*ctx).alloc_window);
        (*ctx).thread = &mut thread;
        (*ctx).error = &mut error;
        (*ctx).alloc_window = vm.jit_allocation_window();
        previous
    };
    let result = unsafe {
        (*ctx).pending_call = CallRequest::resume_interpreter();
        (*ctx).completion = NativeResultPair::success(Value::UNDEFINED);
        (*ctx).completion_destination = u32::MAX;
        crate::native_abi::call_trampoline(ctx)
    };
    unsafe {
        (*ctx).thread = previous_thread;
        (*ctx).error = previous_error;
        (*ctx).alloc_window = previous_window;
    }
    match result.validate(NativeResultDomain::Execution) {
        Some(NativeResultStatus::Success) => Ok(result.payload_value()),
        Some(NativeResultStatus::Throw) => {
            vm.set_pending_uncaught_throw(result.payload_value());
            Err(VmError::Uncaught)
        }
        Some(NativeResultStatus::Fatal) => Err(error.unwrap_or(VmError::InvalidOperand)),
        _ => Err(VmError::InvalidOperand),
    }
}

/// Complete the call an opcode kernel staged, through the real classifying
/// trampoline, and deliver its value to the caller register it names.
pub(crate) fn complete_staged_call(
    vm: &mut Interpreter,
    stack: &mut ActivationStack,
    context: &crate::ExecutionContext,
) -> Result<(), VmError> {
    let request = stack.staged_request_mut().ok_or(VmError::InvalidOperand)?;
    let destination = std::mem::replace(&mut request.return_destination, u32::MAX);
    let value = vm.execute_prepared_call(Some(context), stack)?;
    if let Ok(register) = u16::try_from(destination) {
        let top = stack.len().checked_sub(1).ok_or(VmError::InvalidOperand)?;
        crate::write_register(&mut stack[top], register, value)?;
    }
    Ok(())
}

pub(crate) struct FrameFixture {
    frame: Box<Frame>,
    _slots: Option<Box<[Value]>>,
}

impl From<Frame> for FrameFixture {
    fn from(frame: Frame) -> Self {
        Self {
            frame: Box::new(frame),
            _slots: None,
        }
    }
}

impl std::ops::Deref for FrameFixture {
    type Target = Frame;
    fn deref(&self) -> &Frame {
        &self.frame
    }
}
impl std::ops::DerefMut for FrameFixture {
    fn deref_mut(&mut self) -> &mut Frame {
        &mut self.frame
    }
}

impl Interpreter {
    pub(crate) fn test_frame_for_function(
        &mut self,
        function: &otter_bytecode::Function,
    ) -> Result<FrameFixture, VmError> {
        let count = usize::from(function.param_count)
            + usize::from(function.locals)
            + usize::from(function.scratch);
        let mut slots = vec![Value::UNDEFINED; count].into_boxed_slice();
        let frame = Frame::for_function(
            function,
            None,
            Value::function(function.id),
            Value::UNDEFINED,
            crate::RegisterWindow::attached(slots.as_mut_ptr(), count),
        );
        Ok(FrameFixture {
            frame: Box::new(frame),
            _slots: Some(slots),
        })
    }
}

pub(crate) struct FrameChainFixture {
    view: ActivationStack,
    context: Box<JitCtx>,
    frames: Vec<FrameFixture>,
}

impl FrameChainFixture {
    pub(crate) fn new() -> Self {
        let mut context = Box::new(JitCtx {
            thread: std::ptr::null_mut(),
            native_frame: std::ptr::null_mut(),
            error: std::ptr::null_mut(),
            generated_depth_limit: u64::MAX,
            global_this_offset: std::ptr::null(),
            native_stack_limit: 0,
            generated_feedback_clean: 1,
            alloc_window: crate::jit::JitMachineAllocationWindow::disabled(),
            runtime_stats: std::ptr::null_mut(),
            pending_call: crate::native_abi::CallRequest::EMPTY,
            completion: crate::native_abi::NativeResultPair::success(Value::UNDEFINED),
            completion_destination: u32::MAX,
            completion_generation: 0,
        });
        let mut view = ActivationStack::new();
        // SAFETY: the box keeps the context stationary for the fixture's life.
        unsafe { view.bind_context(&mut *context) };
        Self {
            view,
            context,
            frames: Vec::new(),
        }
    }

    pub(crate) fn push(&mut self, frame: impl Into<FrameFixture>) {
        let mut frame = frame.into();
        frame.caller = self.context.native_frame as u64;
        frame.depth = self.view.len() as u32 + 1;
        self.context.native_frame = &mut *frame.frame;
        self.view.clear_completion();
        self.frames.push(frame);
    }

    pub(crate) fn pop(&mut self) -> Option<FrameFixture> {
        let frame = self.frames.pop()?;
        self.context.native_frame = frame.caller_frame();
        self.view.clear_completion();
        Some(frame)
    }
}

impl std::ops::Deref for FrameChainFixture {
    type Target = ActivationStack;
    fn deref(&self) -> &ActivationStack {
        &self.view
    }
}
impl std::ops::DerefMut for FrameChainFixture {
    fn deref_mut(&mut self) -> &mut ActivationStack {
        &mut self.view
    }
}

impl std::fmt::Debug for FrameChainFixture {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FrameChainFixture")
            .field("depth", &self.view.len())
            .finish()
    }
}
