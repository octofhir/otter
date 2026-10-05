//! Checked access to the common JavaScript activation record.
//!
//! # Contents
//! - [`ActiveFrameRef`] and [`ActiveFrameMut`] provide scalar frame access.
//! - Window validation and checked single-slot operations.
//! - Collector traversal of frame fields and published tagged windows.
//!
//! # Invariants
//! - Every execution tier uses the same [`Frame`] and register descriptor.
//! - Views retain raw window descriptors, never slices across allocation or
//!   JavaScript reentry. Each read or write touches one checked slot.
//! - A raw-pointer view requires stable, initialized frame storage for its
//!   lifetime. Mutable access requires exclusive logical mutator ownership.
//! - Every activation names its actual arguments explicitly: a generated
//!   caller's outgoing span or the trampoline's copy. An optimizing frame has
//!   no register window; its root homes are described separately.
//! - GC rewrites the published slots in place; copied values must be rooted
//!   separately when retained across an allocation.
//!
//! # See also
//! - [`crate::native_abi::Frame`] for the machine-visible layout.
//! - [`crate::frame_state`] for suspended execution state.

use crate::{
    Frame, Value, VmError,
    native_abi::{NativeFrameKind, VmFrameHeader},
};
use otter_gc::raw::SlotVisitor;
use std::{fmt, marker::PhantomData, mem, ptr::NonNull};

/// Rejected native-frame pointer or window descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveFrameError {
    /// The native-frame pointer itself was null.
    NullNativeFrame,
    /// The native-frame pointer did not satisfy [`Frame`]'s alignment.
    MisalignedNativeFrame,
    /// A non-empty register window had no base address.
    MissingRegisterWindow,
    /// The register-window base was not aligned for [`Value`].
    MisalignedRegisterWindow,
    /// A 64-bit ABI address cannot be represented by this target's pointer size.
    AddressOutOfRange,
    /// The described allocation range overflows the target address space.
    WindowOutOfRange,
}

impl fmt::Display for ActiveFrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::NullNativeFrame => "native frame pointer is null",
            Self::MisalignedNativeFrame => "native frame pointer is misaligned",
            Self::MissingRegisterWindow => "non-empty native register window has no base",
            Self::MisalignedRegisterWindow => "native register window is misaligned",
            Self::AddressOutOfRange => "native ABI address is outside the target pointer range",
            Self::WindowOutOfRange => "native ABI window is outside the target address range",
        };
        f.write_str(message)
    }
}

impl std::error::Error for ActiveFrameError {}

/// Validated raw window owned by a published activation.
///
/// This is intentionally not a Rust slice. Runtime operations commonly keep
/// an [`ActiveFrameMut`] while reconstructing the owning [`crate::Interpreter`]
/// and may allocate or re-enter JavaScript before committing a result. A slice
/// stored here would assert an exclusive borrow across that work even though GC
/// is allowed to inspect and relocate values in the published register stack.
#[derive(Debug, Clone, Copy)]
struct NativeWindow<T> {
    base: NonNull<T>,
    len: usize,
}

impl<T> NativeWindow<T> {
    #[inline]
    const fn empty() -> Self {
        Self {
            base: NonNull::dangling(),
            len: 0,
        }
    }
}

impl<T: Copy> NativeWindow<T> {
    #[inline]
    fn read(self, index: usize) -> Option<T> {
        if index >= self.len {
            return None;
        }
        // SAFETY: construction validates the range and the native activation
        // publication contract keeps every element initialized and live.
        Some(unsafe { self.base.as_ptr().add(index).read() })
    }

    #[inline]
    fn write(self, index: usize, value: T) -> bool {
        if index >= self.len {
            return false;
        }
        // SAFETY: as `read`; mutable ActiveFrame access carries exclusive
        // logical ownership for this single-slot commit. No Rust reference to
        // the window remains live before or after this store.
        unsafe { self.base.as_ptr().add(index).write(value) };
        true
    }
}

/// Validate the register window and the actual-argument span the frame names.
fn checked_register_windows(
    frame: &Frame,
) -> Result<(NativeWindow<Value>, NativeWindow<Value>), ActiveFrameError> {
    let register_count = usize::from(frame.header.register_count);
    if register_count > frame.registers.len() {
        return Err(ActiveFrameError::WindowOutOfRange);
    }
    let registers = checked_window::<Value>(
        frame.register_base(),
        register_count,
        ActiveFrameError::MissingRegisterWindow,
        ActiveFrameError::MisalignedRegisterWindow,
    )?;
    let incoming = checked_window::<Value>(
        frame.actuals as u64,
        frame.incoming_argument_count() as usize,
        ActiveFrameError::MissingRegisterWindow,
        ActiveFrameError::MisalignedRegisterWindow,
    )?;
    Ok((registers, incoming))
}

/// Shared slot-scoped access to one live activation.
#[derive(Debug)]
pub struct ActiveFrameRef<'a> {
    frame: NonNull<Frame>,
    registers: NativeWindow<Value>,
    incoming: NativeWindow<Value>,
    _lifetime: PhantomData<&'a Frame>,
}

/// Exclusive logical mutator access to one live activation.
#[derive(Debug)]
pub struct ActiveFrameMut<'a> {
    frame: NonNull<Frame>,
    registers: NativeWindow<Value>,
    incoming: NativeWindow<Value>,
    _lifetime: PhantomData<&'a mut Frame>,
}

impl<'a> ActiveFrameRef<'a> {
    /// Borrow an initialized frame and its published windows.
    #[must_use]
    pub fn from_frame(frame: &'a Frame) -> Self {
        // SAFETY: the frame owner keeps its initialized windows live.
        unsafe { Self::from_ptr(frame) }.expect("initialized frame windows")
    }

    /// Open a view over a published frame without retaining Rust references.
    ///
    /// # Safety
    /// `frame` and its initialized windows must remain live and stable for
    /// `'a`. No semantic writer may race this view. Collection may rewrite
    /// slots only under the engine's exclusive mutator protocol.
    pub unsafe fn from_ptr(frame: *const Frame) -> Result<Self, ActiveFrameError> {
        validate_frame_pointer(frame)?;
        // SAFETY: pointer validity is the caller's contract; this reference
        // ends after copying the scalar window descriptors.
        let (registers, incoming) = checked_register_windows(unsafe { &*frame })?;
        Ok(Self {
            frame: NonNull::new(frame.cast_mut()).expect("validated frame"),
            registers,
            incoming,
            _lifetime: PhantomData,
        })
    }

    /// Copy the tier-independent execution header.
    #[must_use]
    pub fn header(&self) -> VmFrameHeader {
        // SAFETY: one scalar read from the live record.
        unsafe { self.frame.as_ref().header }
    }
    /// Global function identity.
    #[must_use]
    pub fn function_id(&self) -> u32 {
        self.header().function_id
    }
    /// Canonical instruction-index resume PC.
    #[must_use]
    pub fn pc(&self) -> u32 {
        self.header().pc
    }
    /// Number of initialized tagged registers.
    #[must_use]
    pub fn register_count(&self) -> usize {
        self.registers.len
    }
    /// Raw window base; callers must not retain a slice across a safepoint.
    #[must_use]
    pub fn register_base_ptr(&self) -> *const Value {
        self.registers.base.as_ptr()
    }
    /// Read one checked register.
    pub fn read(&self, register: u16) -> Result<Value, VmError> {
        self.registers
            .read(usize::from(register))
            .ok_or(VmError::InvalidOperand)
    }
    /// Actual argument count in the contiguous published window.
    #[must_use]
    pub fn incoming_argument_count(&self) -> usize {
        // SAFETY: one scalar read from the live record.
        unsafe { self.frame.as_ref().incoming_argument_count() as usize }
    }
    /// The frame's cached arguments identity.
    pub(crate) fn native_arguments_object(&self) -> Option<crate::object::JsObject> {
        // SAFETY: one scalar read from a traced frame slot.
        unsafe { self.frame.as_ref().arguments_object() }
    }
    /// Read one checked actual argument.
    pub fn incoming_argument(&self, index: usize) -> Result<Value, VmError> {
        self.incoming.read(index).ok_or(VmError::InvalidOperand)
    }
    /// Exact running function object.
    #[must_use]
    pub fn self_value(&self) -> Value {
        // SAFETY: one scalar read from a traced frame slot.
        unsafe { self.frame.as_ref().self_value() }
    }
    /// Current receiver binding.
    #[must_use]
    pub fn this_value(&self) -> Value {
        // SAFETY: one scalar read from a traced frame slot.
        unsafe { self.frame.as_ref().this_value() }
    }
    /// Current `new.target` binding.
    #[must_use]
    pub fn new_target_value(&self) -> Value {
        // SAFETY: one scalar read from a traced frame slot.
        unsafe { self.frame.as_ref().new_target() }
    }
    /// Context of the exact running closure.
    #[must_use]
    pub fn closure_context(&self, heap: &otter_gc::GcHeap) -> Value {
        closure_context_of(self.self_value(), heap)
    }

    /// Trace the published register and actual-argument windows exactly once.
    pub(crate) fn trace_stack_register_slots(&self, visitor: &mut SlotVisitor<'_>) {
        for window in [self.registers, self.incoming] {
            for index in 0..window.len {
                // SAFETY: initialized published slots remain live. Collector
                // ownership permits a short in-place relocation update.
                let slot = unsafe { window.base.as_ptr().add(index) };
                unsafe { (&mut *slot).trace_value_slot_mut(visitor) };
            }
        }
    }

    /// Trace SELF, receiver, new.target, and the cached arguments object.
    pub(crate) fn trace_non_register_slots(&self, visitor: &mut SlotVisitor<'_>) {
        // SAFETY: this view names a live published frame, and the collector
        // owns the short relocation writes during this root walk.
        unsafe { Frame::trace_fields(self.frame.as_ptr(), visitor) };
    }
}

impl<'a> ActiveFrameMut<'a> {
    /// Borrow one initialized activation for logical mutator access.
    #[must_use]
    pub fn from_frame(frame: &'a mut Frame) -> Self {
        // SAFETY: exclusive frame ownership retains initialized windows.
        unsafe { Self::from_ptr(frame) }.expect("initialized frame windows")
    }

    /// Open a mutable view over a published frame.
    ///
    /// # Safety
    /// The record and its initialized windows must remain stable and live for
    /// `'a`, with exclusive logical mutator ownership. Collector rewrites use
    /// the engine's safepoint protocol. No owner may reclaim the frame or its
    /// windows while the view is live.
    pub unsafe fn from_ptr(frame: *mut Frame) -> Result<Self, ActiveFrameError> {
        // SAFETY: the caller supplies the stronger exclusive contract.
        let shared = unsafe { ActiveFrameRef::from_ptr(frame) }?;
        Ok(Self {
            frame: shared.frame,
            registers: shared.registers,
            incoming: shared.incoming,
            _lifetime: PhantomData,
        })
    }
    /// Shared reborrow of the same frame and windows.
    #[must_use]
    pub fn as_ref(&self) -> ActiveFrameRef<'_> {
        ActiveFrameRef {
            frame: self.frame,
            registers: self.registers,
            incoming: self.incoming,
            _lifetime: PhantomData,
        }
    }
    /// Copy the current execution header.
    #[must_use]
    pub fn header(&self) -> VmFrameHeader {
        self.as_ref().header()
    }
    /// Global function identity.
    #[must_use]
    pub fn function_id(&self) -> u32 {
        self.header().function_id
    }
    /// Canonical instruction-index resume PC.
    #[must_use]
    pub fn pc(&self) -> u32 {
        self.header().pc
    }
    /// Publish a canonical resume PC.
    pub fn set_pc(&mut self, pc: u32) {
        // SAFETY: one scalar write under exclusive logical mutator ownership.
        unsafe {
            self.frame.as_mut().header.pc = pc;
        }
    }
    /// Advance the PC, rejecting integer overflow.
    pub fn advance_pc(&mut self) -> Result<(), VmError> {
        self.set_pc(self.pc().checked_add(1).ok_or(VmError::InvalidOperand)?);
        Ok(())
    }
    /// Number of initialized tagged registers.
    #[must_use]
    pub fn register_count(&self) -> usize {
        self.registers.len
    }
    /// Number of published actual arguments.
    #[must_use]
    pub fn incoming_argument_count(&self) -> usize {
        self.as_ref().incoming_argument_count()
    }
    /// Read one actual argument.
    pub fn incoming_argument(&self, index: usize) -> Result<Value, VmError> {
        self.as_ref().incoming_argument(index)
    }
    /// Publish the frame's single arguments identity.
    pub(crate) fn set_native_arguments_object(
        &mut self,
        object: crate::object::JsObject,
    ) -> Result<(), VmError> {
        // SAFETY: one non-allocating write into a traced slot.
        unsafe {
            self.frame.as_mut().set_arguments_object(Some(object));
        }
        Ok(())
    }
    /// Raw window base; no Rust slice may span allocation or JS reentry.
    #[must_use]
    pub fn register_base_ptr(&self) -> *mut Value {
        self.registers.base.as_ptr()
    }
    /// Bounds-checked address of a traced register slot.
    pub(crate) fn register_slot_ptr(&self, register: u16) -> Result<*const Value, VmError> {
        if usize::from(register) >= self.register_count() {
            return Err(VmError::InvalidOperand);
        }
        // SAFETY: the index is within the initialized window.
        Ok(unsafe { self.register_base_ptr().add(usize::from(register)) }.cast_const())
    }
    /// Read one checked register.
    pub fn read(&self, register: u16) -> Result<Value, VmError> {
        self.as_ref().read(register)
    }
    /// Write one checked register.
    pub fn write(&mut self, register: u16, value: Value) -> Result<(), VmError> {
        self.registers
            .write(usize::from(register), value)
            .then_some(())
            .ok_or(VmError::InvalidOperand)
    }
    /// Exact running function object.
    #[must_use]
    pub fn self_value(&self) -> Value {
        self.as_ref().self_value()
    }
    /// Replace the exact running function object.
    pub fn set_self_value(&mut self, value: Value) {
        // SAFETY: one traced-slot write under logical mutator ownership.
        unsafe {
            self.frame.as_mut().set_self_value(value);
        }
    }
    /// Current receiver binding.
    #[must_use]
    pub fn this_value(&self) -> Value {
        self.as_ref().this_value()
    }
    /// Publish the exact initialized own DerivedThis context under this frame's
    /// logical mutator ownership. The semantic CreateContext owner proves its
    /// descriptor before this noncollecting identity-root store.
    pub(crate) fn publish_derived_this_context(
        &mut self,
        source_function_id: u32,
        context: crate::context::ContextHandle,
    ) {
        // SAFETY: this view owns the live record for this single noallocation store.
        unsafe {
            self.frame
                .as_mut()
                .publish_derived_this_context(source_function_id, context);
        }
    }

    /// Replace the receiver binding.
    pub fn set_this_value(&mut self, value: Value) {
        // SAFETY: one traced-slot write under logical mutator ownership.
        unsafe {
            self.frame.as_mut().set_this_value(value);
        }
    }
    /// Current new.target binding.
    #[must_use]
    pub fn new_target_value(&self) -> Value {
        self.as_ref().new_target_value()
    }
    /// Context of the running closure.
    #[must_use]
    pub fn closure_context(&self, heap: &otter_gc::GcHeap) -> Value {
        self.as_ref().closure_context(heap)
    }
    /// Switch dispatch tier while retaining the same activation and windows.
    pub fn enter_interpreter(&mut self) -> Result<(), VmError> {
        // SAFETY: non-allocating execution-mode update.
        unsafe { self.frame.as_mut().enter_interpreter() }
            .then_some(())
            .ok_or(VmError::InvalidOperand)
    }
    /// Switch to a compiled tier on the same activation.
    pub fn enter_compiled(&mut self, kind: NativeFrameKind) -> Result<(), VmError> {
        // SAFETY: non-allocating execution-mode update.
        unsafe { self.frame.as_mut().enter_compiled(kind) }
            .then_some(())
            .ok_or(VmError::InvalidOperand)
    }
}

/// SELF's closure context: `undefined` for a non-closure SELF, which only a
/// function whose code never reads its context may run with.
fn closure_context_of(self_value: Value, heap: &otter_gc::GcHeap) -> Value {
    match self_value.as_closure(heap) {
        Some(closure) => closure.context(heap),
        None => {
            debug_assert!(
                self_value.is_function_id(),
                "SELF is the exact closure or a bare function value"
            );
            Value::undefined()
        }
    }
}

fn validate_frame_pointer(frame: *const Frame) -> Result<(), ActiveFrameError> {
    if frame.is_null() {
        return Err(ActiveFrameError::NullNativeFrame);
    }
    if !(frame as usize).is_multiple_of(mem::align_of::<Frame>()) {
        return Err(ActiveFrameError::MisalignedNativeFrame);
    }
    Ok(())
}

fn checked_window<T>(
    address: u64,
    count: usize,
    missing: ActiveFrameError,
    misaligned: ActiveFrameError,
) -> Result<NativeWindow<T>, ActiveFrameError> {
    if count == 0 {
        return Ok(NativeWindow::empty());
    }
    let address = usize::try_from(address).map_err(|_| ActiveFrameError::AddressOutOfRange)?;
    if address == 0 {
        return Err(missing);
    }
    if !address.is_multiple_of(mem::align_of::<T>()) {
        return Err(misaligned);
    }
    let byte_len = count
        .checked_mul(mem::size_of::<T>())
        .filter(|&len| len <= isize::MAX as usize)
        .ok_or(ActiveFrameError::WindowOutOfRange)?;
    address
        .checked_add(byte_len)
        .ok_or(ActiveFrameError::WindowOutOfRange)?;
    Ok(NativeWindow {
        // SAFETY: zero was rejected above and alignment was validated.
        base: unsafe { NonNull::new_unchecked(address as *mut T) },
        len: count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::native_abi::{NativeFrameFlags, NativeFrameKind};

    fn header(register_count: u16) -> VmFrameHeader {
        VmFrameHeader {
            function_id: 7,
            pc: 3,
            register_count,
            kind: NativeFrameKind::Baseline,
            flags: NativeFrameFlags::empty(),
        }
    }

    fn materialized_frame(slots: &mut [Value]) -> Frame {
        Frame::new(
            header(slots.len() as u16),
            slots.as_mut_ptr() as u64,
            Value::function(7),
            Value::number_i32(9),
        )
    }

    #[test]
    fn native_window_access_is_slot_scoped_across_gc_relocation() {
        let mut native_slots = [Value::number_i32(1), Value::undefined()];
        let native_base = native_slots.as_mut_ptr();
        let mut native = Frame::new(
            header(native_slots.len() as u16),
            native_base as u64,
            Value::function(7),
            Value::number_i32(9),
        );
        {
            // SAFETY: `native` and `native_slots` remain exclusively live for
            // the view and match the published descriptors above.
            let mut active = unsafe { ActiveFrameMut::from_ptr(&mut native) }.unwrap();
            assert_eq!(active.register_base_ptr(), native_base);
            assert_eq!(active.register_count(), 2);
            assert_eq!(active.read(0).unwrap(), Value::number_i32(1));
            active.write(1, Value::number_i32(2)).unwrap();

            // Model the collector's in-place relocation update between two
            // semantic operations. ActiveFrame stores only a raw descriptor,
            // so no `&mut [Value]` borrow spans this external slot rewrite.
            // SAFETY: `native_base` names initialized published storage and
            // this test performs the collector-authorized single-slot update.
            unsafe { native_base.write(Value::number_i32(41)) };
            assert_eq!(active.read(0).unwrap(), Value::number_i32(41));

            active.advance_pc().unwrap();
            active.set_self_value(Value::function(17));
            active.enter_interpreter().unwrap();
            assert_eq!(active.header().kind, NativeFrameKind::Interpreter);
            assert_eq!(active.read(1).unwrap(), Value::number_i32(2));
            active.enter_compiled(NativeFrameKind::Optimizing).unwrap();
        }
        assert_eq!(native_slots[1], Value::number_i32(2));
        assert_eq!(native.header.pc, 4);
        assert_eq!(native.self_value(), Value::function(17));
        assert_eq!(native.header.kind, NativeFrameKind::Optimizing);
        assert_eq!(native.register_base(), native_base as u64);
    }

    #[test]
    fn actual_arguments_keep_their_address_across_register_window_changes() {
        let mut slots = [
            Value::number_i32(1),
            Value::UNDEFINED,
            Value::UNDEFINED,
            Value::UNDEFINED,
            Value::number_i32(30),
            Value::number_i32(40),
        ];
        let mut frame = Frame::new(
            header(4),
            slots.as_mut_ptr() as u64,
            Value::function(7),
            Value::UNDEFINED,
        );

        let actuals = unsafe { slots.as_mut_ptr().add(4) };
        frame.set_incoming_arguments(actuals, 2);
        frame.header.register_count = 1;
        for initialized in [1, 4] {
            frame.header.register_count = initialized;
            let active = unsafe { ActiveFrameRef::from_ptr(&frame) }.unwrap();
            assert_eq!(active.register_count(), initialized as usize);
            assert_eq!(active.incoming_argument(0).unwrap(), Value::number_i32(30));
            assert_eq!(active.incoming_argument(1).unwrap(), Value::number_i32(40));
        }
        frame.header.register_count = 5;
        assert!(matches!(
            unsafe { ActiveFrameRef::from_ptr(&frame) },
            Err(ActiveFrameError::WindowOutOfRange)
        ));
    }

    #[test]
    fn incoming_argument_window_is_named_by_the_frame_and_traced() {
        let mut slots = [
            Value::number_i32(1),
            Value::number_i32(2),
            Value::number_i32(30),
            Value::number_i32(40),
            Value::number_i32(50),
        ];
        let mut native = Frame::new(
            header(2),
            slots.as_mut_ptr() as u64,
            Value::function(7),
            Value::undefined(),
        );

        let actuals = unsafe { slots.as_mut_ptr().add(2) };
        native.set_incoming_arguments(actuals, 3);
        // SAFETY: the frame, its two registers, and the three published
        // arguments remain live for the view.
        let active = unsafe { ActiveFrameRef::from_ptr(&native) }.unwrap();
        assert_eq!(active.register_count(), 2);
        assert_eq!(active.incoming_argument_count(), 3);
        assert_eq!(active.incoming_argument(0).unwrap(), Value::number_i32(30));
        assert_eq!(active.incoming_argument(2).unwrap(), Value::number_i32(50));
        assert!(matches!(
            active.incoming_argument(3),
            Err(VmError::InvalidOperand)
        ));
        let mut visited = 0;
        active.trace_stack_register_slots(&mut |_slot| {
            visited += 1;
        });
        assert_eq!(visited, 0, "tagged integers carry no heap slot");
    }

    #[test]
    fn native_pointer_validation_rejects_invalid_descriptors() {
        // SAFETY: constructor validates null before dereferencing it.
        let null = unsafe { ActiveFrameMut::from_ptr(std::ptr::null_mut()) };
        assert!(matches!(null, Err(ActiveFrameError::NullNativeFrame)));

        let mut native = Frame::new(header(1), 0, Value::function(7), Value::undefined());
        // SAFETY: the frame record is valid; its deliberately missing register
        // window is rejected before a slice is formed.
        let missing = unsafe { ActiveFrameMut::from_ptr(&mut native) };
        assert!(matches!(
            missing,
            Err(ActiveFrameError::MissingRegisterWindow)
        ));
    }

    #[test]
    fn native_single_slot_access_is_bounds_checked() {
        let mut slots = [Value::undefined()];
        let mut native = Frame::new(
            header(slots.len() as u16),
            slots.as_mut_ptr() as u64,
            Value::function(7),
            Value::undefined(),
        );
        // SAFETY: frame and its one initialized slot remain live for the view.
        let mut active = unsafe { ActiveFrameMut::from_ptr(&mut native) }.unwrap();
        assert!(matches!(active.read(1), Err(VmError::InvalidOperand)));
        assert!(matches!(
            active.write(1, Value::undefined()),
            Err(VmError::InvalidOperand)
        ));
    }

    #[test]
    fn native_arguments_cache_is_rewritten_in_place() {
        let mut slots = [Value::undefined()];
        let mut native = Frame::new(
            header(1),
            slots.as_mut_ptr() as u64,
            Value::function(7),
            Value::undefined(),
        );
        // SAFETY: synthetic offsets are only compared/rewritten, never dereferenced.
        let before = unsafe { crate::object::JsObject::from_offset(0x1000) };
        let after = unsafe { crate::object::JsObject::from_offset(0x2000) };
        native.set_arguments_object(Some(before));
        // SAFETY: the frame and initialized window remain live throughout tracing.
        let active = unsafe { ActiveFrameRef::from_ptr(&native) }.unwrap();
        let mut rewrites = 0;
        active.trace_non_register_slots(&mut |slot| unsafe {
            if (*slot).0 == before.offset() {
                slot.write(after.raw());
                rewrites += 1;
            }
        });
        assert_eq!(rewrites, 1);
        assert_eq!(native.arguments_object(), Some(after));
    }

    #[test]
    fn closure_context_reads_self_through_checked_views() {
        let mut interp = crate::Interpreter::new().expect("fixture interpreter bootstrap");
        interp
            .with_handle_scope(|interp, scope| {
                let context = crate::context::alloc_context_with_roots(
                    &mut interp.gc_heap,
                    crate::context::ContextShape {
                        scope_function_id: 7,
                        scope_index: 0,
                        slot_count: 1,
                        has_extension: false,
                    },
                    Value::undefined(),
                    |_| false,
                    &mut |_| {},
                )?;
                let context = interp.scoped_value(scope, Value::context(context));
                let parent = interp.escape_scoped(context);
                let closure =
                    crate::closure::alloc_closure(&mut interp.gc_heap, 7, parent, None, None)?;
                let mut slots = [Value::undefined()];
                let mut frame = Frame::new(
                    header(1),
                    slots.as_mut_ptr() as u64,
                    Value::closure(closure),
                    Value::undefined(),
                );
                // SAFETY: the initialized frame and its window remain stationary.
                let active = unsafe { ActiveFrameRef::from_ptr(&frame) }.unwrap();
                assert_eq!(
                    active.closure_context(&interp.gc_heap),
                    interp.escape_scoped(context)
                );
                assert_eq!(
                    ActiveFrameRef::from_frame(&frame).closure_context(&interp.gc_heap),
                    interp.escape_scoped(context)
                );
                frame.set_self_value(Value::function(7));
                assert!(
                    ActiveFrameRef::from_frame(&frame)
                        .closure_context(&interp.gc_heap)
                        .is_undefined()
                );
                Ok::<(), otter_gc::OutOfMemory>(())
            })
            .expect("rooted frame fixture");
    }

    #[test]
    fn parked_frames_preserve_call_bindings_and_arguments_identity() {
        let mut slots = [Value::number_i32(5)];
        let mut frame = materialized_frame(&mut slots);
        frame.self_value = Value::function(41);
        frame.set_new_target(Value::function(42));
        frame.set_derived_constructor();
        // SAFETY: the synthetic handle is compared only, never dereferenced.
        let arguments = unsafe { crate::JsObject::from_offset(0x1000) };
        frame.set_arguments_object(Some(arguments));
        let parked = crate::frame_state::ParkedFrameState::copy_from_active(&frame);
        let restored = parked.into_prepared();
        assert_eq!(restored.self_value, Value::function(41));
        assert_eq!(restored.new_target_value, Value::function(42));
        assert!(
            restored
                .header
                .flags
                .contains(NativeFrameFlags::DERIVED_CONSTRUCTOR)
        );
        assert_eq!(restored.arguments_object, arguments);
        assert_eq!(restored.initial_registers[0], Value::number_i32(5));
    }
}
