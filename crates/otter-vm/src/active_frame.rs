//! Representation-neutral access to a live JavaScript activation.
//!
//! [`NativeFrame`] plus its published register window is the canonical
//! activation shared by interpreter, baseline, and optimizing tiers. Tier
//! switches mutate only execution metadata and preserve that window. A
//! materialized [`Frame`] remains the cold interpreter-owned representation.
//! Runtime semantics should not otherwise know which representation they
//! received.
//!
//! # Contents
//! - [`ActiveFrameRef`] — shared access to common frame state.
//! - [`ActiveFrameMut`] — register, binding, PC, and frame-state mutation.
//! - [`ActiveFrameStorage`] — the active physical representation.
//! - [`ActiveFrameError`] — validation failures at the native ABI boundary.
//!
//! # Invariants
//! - Native views are created at one audited `unsafe` boundary. Their register
//!   descriptors must refer to published, initialized storage for the whole
//!   view lifetime.
//! - A stack-owned frame may publish its actual arguments as a third tagged
//!   window directly after the register window; it is traced with the
//!   registers and read only through checked single-slot accessors.
//! - Native windows stay raw inside the view. Safe operations create no slice
//!   whose borrow can survive an allocating or reentrant VM call; reads return
//!   copied handles and writes touch exactly one checked slot.
//! - Interpreter entry on a native activation is zero-copy: register base,
//!   SELF, and `this` remain authoritative in [`NativeFrame`].
//! - An activation's incoming context is its SELF closure's context
//!   ([`ActiveFrameRef::closure_context`]); frames store no binding storage.
//! - A mutable native view has exclusive logical ownership of its frame record
//!   and tagged windows. It deliberately does not manufacture long-lived Rust
//!   references to register-stack storage: GC and reentrant runtime work may
//!   revisit that storage through the owning interpreter between operations.
//! - PC advancement is checked and register access is bounds checked for both
//!   representations.
//!
//! # See also
//! - [`crate::frame_state::Frame`] — materialized interpreter state.
//! - [`crate::native_abi::NativeFrame`] — stable machine-visible record.
//! - [`crate::register_stack`] — published native register storage.

use std::{fmt, mem, ptr::NonNull};

use otter_gc::raw::{RawGc, SlotVisitor};

use crate::{
    Frame, Value, VmError,
    native_abi::{NativeFrame, NativeFrameFlags, NativeFrameKind, VmFrameHeader},
};

/// Physical representation backing an active frame view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveFrameStorage {
    /// Full interpreter [`Frame`] published on a `ActivationStack`.
    Materialized,
    /// Machine-visible [`NativeFrame`] plus its published register window.
    Native,
}

/// Rejected native-frame pointer or window descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveFrameError {
    /// The native-frame pointer itself was null.
    NullNativeFrame,
    /// The native-frame pointer did not satisfy [`NativeFrame`]'s alignment.
    MisalignedNativeFrame,
    /// A non-empty register window had no base address.
    MissingRegisterWindow,
    /// The register-window base was not aligned for [`Value`].
    MisalignedRegisterWindow,
    /// A 64-bit ABI address cannot be represented by this target's pointer size.
    AddressOutOfRange,
    /// The described allocation range overflows the target address space.
    WindowOutOfRange,
    /// The frame publishes an actual-argument window without owning its
    /// register window on the generated-code stack.
    UnownedIncomingArguments,
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
            Self::UnownedIncomingArguments => {
                "native incoming-argument window requires stack-owned registers"
            }
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

#[derive(Debug)]
struct NativeFrameRef {
    frame: NonNull<NativeFrame>,
    registers: NativeWindow<Value>,
    incoming: NativeWindow<Value>,
}

#[derive(Debug)]
struct NativeFrameMut {
    frame: NonNull<NativeFrame>,
    registers: NativeWindow<Value>,
    incoming: NativeWindow<Value>,
}

/// Validate the register window and the actual-argument window that a
/// generated caller publishes directly after it.
fn checked_register_windows(
    frame: &NativeFrame,
) -> Result<(NativeWindow<Value>, NativeWindow<Value>), ActiveFrameError> {
    let register_count = usize::from(frame.header.register_count);
    let registers = checked_window::<Value>(
        frame.register_base,
        register_count,
        ActiveFrameError::MissingRegisterWindow,
        ActiveFrameError::MisalignedRegisterWindow,
    )?;
    let Some(argument_count) = frame.incoming_argument_count() else {
        return Ok((registers, NativeWindow::empty()));
    };
    if !frame
        .header
        .flags
        .contains(NativeFrameFlags::STACK_REGISTERS)
    {
        return Err(ActiveFrameError::UnownedIncomingArguments);
    }
    let register_bytes = (register_count as u64)
        .checked_mul(mem::size_of::<Value>() as u64)
        .ok_or(ActiveFrameError::WindowOutOfRange)?;
    let incoming_base = frame
        .register_base
        .checked_add(register_bytes)
        .ok_or(ActiveFrameError::WindowOutOfRange)?;
    let incoming = checked_window::<Value>(
        incoming_base,
        argument_count as usize,
        ActiveFrameError::MissingRegisterWindow,
        ActiveFrameError::MisalignedRegisterWindow,
    )?;
    Ok((registers, incoming))
}

#[derive(Debug)]
enum ActiveFrameRefInner<'a> {
    Materialized { frame: &'a Frame, new_target: Value },
    Native(NativeFrameRef),
}

#[derive(Debug)]
enum ActiveFrameMutInner<'a> {
    Materialized {
        frame: &'a mut Frame,
        new_target: Value,
    },
    Native(NativeFrameMut),
}

/// Shared, representation-neutral access to one active JS frame.
#[derive(Debug)]
pub struct ActiveFrameRef<'a> {
    inner: ActiveFrameRefInner<'a>,
}

/// Exclusive, representation-neutral access to one active JS frame.
#[derive(Debug)]
pub struct ActiveFrameMut<'a> {
    inner: ActiveFrameMutInner<'a>,
}

impl<'a> ActiveFrameRef<'a> {
    /// Wrap a materialized interpreter frame.
    #[must_use]
    pub fn materialized(frame: &'a Frame) -> Self {
        Self::materialized_with_new_target(frame, Value::undefined())
    }

    /// Wrap a materialized interpreter frame and its immutable `new.target`
    /// binding from the legacy cold sidecar.
    #[must_use]
    pub fn materialized_with_new_target(frame: &'a Frame, new_target: Value) -> Self {
        debug_assert_eq!(
            frame.registers.len(),
            usize::from(frame.header.register_count)
        );
        Self {
            inner: ActiveFrameRefInner::Materialized { frame, new_target },
        }
    }

    /// Build a view over a machine-published native frame.
    ///
    /// # Safety
    ///
    /// `frame` must remain valid for `'a`. Its non-empty
    /// register descriptor must point to initialized storage that
    /// remains live for `'a`; their backing allocations must not move. No
    /// semantic writer may race this activation. The boxed value fields must
    /// contain valid [`Value`] bit patterns. The returned view retains only raw
    /// descriptors, so no Rust reference spans a safepoint.
    pub unsafe fn from_native_ptr(frame: *const NativeFrame) -> Result<Self, ActiveFrameError> {
        validate_frame_pointer(frame)?;
        // SAFETY: null and alignment were validated above. The short reference
        // is used only to copy scalar window descriptors.
        let frame_ref = unsafe { &*frame };
        let (registers, incoming) = checked_register_windows(frame_ref)?;
        let frame = NonNull::new(frame.cast_mut()).expect("validated native frame pointer");
        Ok(Self {
            inner: ActiveFrameRefInner::Native(NativeFrameRef {
                frame,
                registers,
                incoming,
            }),
        })
    }

    /// Physical representation backing this view.
    #[must_use]
    pub const fn storage(&self) -> ActiveFrameStorage {
        match self.inner {
            ActiveFrameRefInner::Materialized { .. } => ActiveFrameStorage::Materialized,
            ActiveFrameRefInner::Native(_) => ActiveFrameStorage::Native,
        }
    }

    /// Common frame header.
    #[must_use]
    pub fn header(&self) -> VmFrameHeader {
        match &self.inner {
            ActiveFrameRefInner::Materialized { frame, .. } => frame.header,
            // SAFETY: the native-view contract keeps the frame live. Copying
            // the header ends the raw access before the caller can safepoint.
            ActiveFrameRefInner::Native(native) => unsafe { native.frame.as_ref().header },
        }
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

    /// Number of tagged registers in the published window.
    #[must_use]
    pub fn register_count(&self) -> usize {
        match &self.inner {
            ActiveFrameRefInner::Materialized { frame, .. } => frame.registers.len(),
            ActiveFrameRefInner::Native(native) => native.registers.len,
        }
    }

    /// Raw base of the initialized tagged register window.
    ///
    /// The pointer is a machine-code integration descriptor, not a Rust borrow.
    /// Callers must not turn it into a slice that spans allocating, GC, or
    /// reentrant VM work. Semantic code should prefer [`Self::read`].
    #[must_use]
    pub fn register_base_ptr(&self) -> *const Value {
        match &self.inner {
            ActiveFrameRefInner::Materialized { frame, .. } => {
                frame.registers.as_mut_ptr().cast_const()
            }
            ActiveFrameRefInner::Native(native) => native.registers.base.as_ptr().cast_const(),
        }
    }

    /// Read one tagged register.
    pub fn read(&self, register: u16) -> Result<Value, VmError> {
        match &self.inner {
            ActiveFrameRefInner::Materialized { frame, .. } => frame
                .registers
                .get(usize::from(register))
                .copied()
                .ok_or(VmError::InvalidOperand),
            ActiveFrameRefInner::Native(native) => native
                .registers
                .read(usize::from(register))
                .ok_or(VmError::InvalidOperand),
        }
    }

    /// Number of actual arguments the generated caller published after the
    /// register window, or `None` when this activation keeps them elsewhere.
    #[must_use]
    pub fn incoming_argument_count(&self) -> Option<usize> {
        match &self.inner {
            ActiveFrameRefInner::Materialized { .. } => None,
            // SAFETY: one scalar read under the native-view contract.
            ActiveFrameRefInner::Native(native) => unsafe { native.frame.as_ref() }
                .incoming_argument_count()
                .map(|count| count as usize),
        }
    }

    /// Native cache identity, when this view owns a native arguments slot.
    pub(crate) fn native_arguments_object(&self) -> Option<crate::object::JsObject> {
        match &self.inner {
            ActiveFrameRefInner::Materialized { .. } => None,
            // SAFETY: one scalar read of a published collector-traced slot.
            ActiveFrameRefInner::Native(native) => unsafe {
                native.frame.as_ref().arguments_object()
            },
        }
    }

    /// Read one published actual argument.
    pub fn incoming_argument(&self, index: usize) -> Result<Value, VmError> {
        match &self.inner {
            ActiveFrameRefInner::Materialized { .. } => Err(VmError::InvalidOperand),
            ActiveFrameRefInner::Native(native) => {
                native.incoming.read(index).ok_or(VmError::InvalidOperand)
            }
        }
    }

    /// Running function object's exact SELF value.
    #[must_use]
    pub fn self_value(&self) -> Value {
        match &self.inner {
            ActiveFrameRefInner::Materialized { frame, .. } => frame.self_value,
            // SAFETY: one scalar read under the native-view contract.
            ActiveFrameRefInner::Native(native) => unsafe { native.frame.as_ref().self_value() },
        }
    }

    /// Current `this` binding.
    #[must_use]
    pub fn this_value(&self) -> Value {
        match &self.inner {
            ActiveFrameRefInner::Materialized { frame, .. } => frame.this_value,
            // SAFETY: one scalar read under the native-view contract.
            ActiveFrameRefInner::Native(native) => unsafe { native.frame.as_ref().this_value() },
        }
    }

    /// Current `new.target` binding.
    #[must_use]
    pub fn new_target_value(&self) -> Value {
        match &self.inner {
            ActiveFrameRefInner::Materialized { new_target, .. } => *new_target,
            // SAFETY: one scalar read under the native-view contract.
            ActiveFrameRefInner::Native(native) => unsafe { native.frame.as_ref().new_target() },
        }
    }

    /// The context the running closure was created over: SELF's closure
    /// context, or `undefined` when SELF is not a closure or closes over no
    /// context.
    #[must_use]
    pub fn closure_context(&self, heap: &otter_gc::GcHeap) -> Value {
        closure_context_of(self.self_value(), heap)
    }

    /// Trace and rewrite a native-stack register window.
    ///
    /// Register-arena windows are traced by
    /// [`crate::register_stack::RegisterStack`] and must not call this method.
    /// Callers gate it with
    /// [`crate::native_abi::NativeFrameFlags::STACK_REGISTERS`] so each tagged register is
    /// visited exactly once.
    pub(crate) fn trace_stack_register_slots(&self, visitor: &mut SlotVisitor<'_>) {
        let ActiveFrameRefInner::Native(native) = &self.inner else {
            return;
        };
        for window in [native.registers, native.incoming] {
            for index in 0..window.len {
                // SAFETY: the validated published window remains live until
                // native activation pop. Stop-the-world tracing owns this
                // short in-place relocation update and retains no reference
                // afterward.
                let slot = unsafe { window.base.as_ptr().add(index) };
                unsafe { (&mut *slot).trace_value_slot_mut(visitor) };
            }
        }
    }

    /// Trace non-register GC slots owned by this activation.
    ///
    /// Register-arena windows are traced once through their published prefix;
    /// generated stack windows are handled separately by
    /// [`Self::trace_stack_register_slots`]. This method owns SELF, `this`,
    /// `new.target`, and the lazy arguments object for a native activation; an
    /// interpreter frame delegates to its established frame tracer.
    pub(crate) fn trace_non_register_slots(&self, visitor: &mut SlotVisitor<'_>) {
        match &self.inner {
            ActiveFrameRefInner::Materialized { frame, .. } => frame.trace_frame_slots(visitor),
            ActiveFrameRefInner::Native(native) => {
                let frame = native.frame.as_ptr();
                // SAFETY: Value is transparent over `u64`; native-frame
                // publication guarantees valid boxed bits and stop-the-world
                // tracing owns each short in-place relocation update. No
                // reference to the enclosing NativeFrame is retained.
                for bits in unsafe {
                    [
                        std::ptr::addr_of_mut!((*frame).self_value_bits),
                        std::ptr::addr_of_mut!((*frame).this_value_bits),
                        std::ptr::addr_of_mut!((*frame).new_target_bits),
                    ]
                } {
                    unsafe { (&mut *bits.cast::<Value>()).trace_value_slot_mut(visitor) };
                }
                // The nullable compressed arguments handle is a frame-owned
                // root slot. Rewrite the field itself so later generated
                // operations observe the moved arguments object.
                let arguments = unsafe { std::ptr::addr_of_mut!((*frame).arguments_object) };
                if !unsafe { (*arguments).is_null() } {
                    visitor(arguments.cast::<RawGc>());
                }
            }
        }
    }
}

impl<'a> ActiveFrameMut<'a> {
    /// Wrap a materialized interpreter frame.
    #[must_use]
    pub fn materialized(frame: &'a mut Frame) -> Self {
        Self::materialized_with_new_target(frame, Value::undefined())
    }

    /// Wrap a materialized interpreter frame and its immutable `new.target`
    /// binding from the legacy cold sidecar.
    #[must_use]
    pub fn materialized_with_new_target(frame: &'a mut Frame, new_target: Value) -> Self {
        debug_assert_eq!(
            frame.registers.len(),
            usize::from(frame.header.register_count)
        );
        Self {
            inner: ActiveFrameMutInner::Materialized { frame, new_target },
        }
    }

    /// Build an exclusive view over a machine-published native frame.
    ///
    /// # Safety
    ///
    /// `frame` must remain valid for `'a`. Its
    /// register descriptor must point to initialized, stable storage with
    /// exclusive logical mutator ownership for `'a`. No owner may reclaim or
    /// move the window until the returned view is dropped. Boxed
    /// fields must contain valid [`Value`] bit patterns. The view retains only
    /// raw descriptors; each method opens at most one short scalar reference so
    /// collector root access never aliases a long-lived Rust borrow.
    pub unsafe fn from_native_ptr(frame: *mut NativeFrame) -> Result<Self, ActiveFrameError> {
        validate_frame_pointer(frame.cast_const())?;
        // SAFETY: null and alignment were validated above. The short reference
        // is used only to copy scalar window descriptors.
        let frame_ref = unsafe { &*frame };
        let (registers, incoming) = checked_register_windows(frame_ref)?;
        let frame = NonNull::new(frame).expect("validated native frame pointer");
        Ok(Self {
            inner: ActiveFrameMutInner::Native(NativeFrameMut {
                frame,
                registers,
                incoming,
            }),
        })
    }

    /// Shared reborrow of this active frame.
    #[must_use]
    pub fn as_ref(&self) -> ActiveFrameRef<'_> {
        match &self.inner {
            ActiveFrameMutInner::Materialized { frame, new_target } => {
                ActiveFrameRef::materialized_with_new_target(frame, *new_target)
            }
            ActiveFrameMutInner::Native(native) => ActiveFrameRef {
                inner: ActiveFrameRefInner::Native(NativeFrameRef {
                    frame: native.frame,
                    registers: native.registers,
                    incoming: native.incoming,
                }),
            },
        }
    }

    /// Physical representation backing this view.
    #[must_use]
    pub fn storage(&self) -> ActiveFrameStorage {
        self.as_ref().storage()
    }

    /// Common frame header.
    #[must_use]
    pub fn header(&self) -> VmFrameHeader {
        match &self.inner {
            ActiveFrameMutInner::Materialized { frame, .. } => frame.header,
            // SAFETY: one copied scalar header under the native-view contract.
            ActiveFrameMutInner::Native(native) => unsafe { native.frame.as_ref().header },
        }
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

    /// Set the canonical instruction-index resume PC.
    pub fn set_pc(&mut self, pc: u32) {
        match &mut self.inner {
            ActiveFrameMutInner::Materialized { frame, .. } => frame.header.pc = pc,
            // SAFETY: one scalar write under exclusive logical mutator access.
            ActiveFrameMutInner::Native(native) => unsafe {
                native.frame.as_mut().header.pc = pc;
            },
        }
    }

    /// Advance the canonical PC by one with overflow checking.
    pub fn advance_pc(&mut self) -> Result<(), VmError> {
        let pc = self.pc().checked_add(1).ok_or(VmError::InvalidOperand)?;
        self.set_pc(pc);
        Ok(())
    }

    /// Number of tagged registers in the published window.
    #[must_use]
    pub fn register_count(&self) -> usize {
        match &self.inner {
            ActiveFrameMutInner::Materialized { frame, .. } => frame.registers.len(),
            ActiveFrameMutInner::Native(native) => native.registers.len,
        }
    }

    /// Number of actual arguments the generated caller published after the
    /// register window, or `None` when this activation keeps them elsewhere.
    #[must_use]
    pub fn incoming_argument_count(&self) -> Option<usize> {
        self.as_ref().incoming_argument_count()
    }

    /// Read one published actual argument.
    pub fn incoming_argument(&self, index: usize) -> Result<Value, VmError> {
        self.as_ref().incoming_argument(index)
    }

    /// Publish the single arguments identity of a native activation.
    pub(crate) fn set_native_arguments_object(
        &mut self,
        object: crate::object::JsObject,
    ) -> Result<(), VmError> {
        match &mut self.inner {
            // SAFETY: one non-allocating write under logical mutator ownership.
            ActiveFrameMutInner::Native(native) => unsafe {
                native.frame.as_mut().set_arguments_object(Some(object));
                Ok(())
            },
            ActiveFrameMutInner::Materialized { .. } => Err(VmError::InvalidOperand),
        }
    }

    /// Write one actual-argument slot in an initialized native window.
    /// Generated linkage uses this while the callee frame is still private.
    pub(crate) fn write_incoming_argument(
        &mut self,
        index: usize,
        value: Value,
    ) -> Result<(), VmError> {
        match &mut self.inner {
            ActiveFrameMutInner::Native(native) if native.incoming.write(index, value) => Ok(()),
            _ => Err(VmError::InvalidOperand),
        }
    }

    /// Raw base of the initialized tagged register window.
    ///
    /// This is a native integration descriptor, not an exclusive Rust borrow.
    /// Do not manufacture a slice that spans allocating or reentrant VM work;
    /// use [`Self::read`] and [`Self::write`] for semantic access.
    #[must_use]
    pub fn register_base_ptr(&self) -> *mut Value {
        match &self.inner {
            ActiveFrameMutInner::Materialized { frame, .. } => frame.registers.as_mut_ptr(),
            ActiveFrameMutInner::Native(native) => native.registers.base.as_ptr(),
        }
    }

    /// Address of one tagged register, bounds checked.
    ///
    /// The slot is a traced root of the published activation, so a moving
    /// collection rewrites it in place; allocating kernels read a value they
    /// need after allocation back through this address.
    pub(crate) fn register_slot_ptr(&self, register: u16) -> Result<*const Value, VmError> {
        if usize::from(register) >= self.register_count() {
            return Err(VmError::InvalidOperand);
        }
        // SAFETY: the index is within the published register window.
        Ok(unsafe { self.register_base_ptr().add(usize::from(register)) }.cast_const())
    }

    /// Read one tagged register.
    pub fn read(&self, register: u16) -> Result<Value, VmError> {
        match &self.inner {
            ActiveFrameMutInner::Materialized { frame, .. } => frame
                .registers
                .get(usize::from(register))
                .copied()
                .ok_or(VmError::InvalidOperand),
            ActiveFrameMutInner::Native(native) => native
                .registers
                .read(usize::from(register))
                .ok_or(VmError::InvalidOperand),
        }
    }

    /// Write one tagged register.
    pub fn write(&mut self, register: u16, value: Value) -> Result<(), VmError> {
        match &mut self.inner {
            ActiveFrameMutInner::Materialized { frame, .. } => {
                let slot = frame
                    .registers
                    .get_mut(usize::from(register))
                    .ok_or(VmError::InvalidOperand)?;
                *slot = value;
                Ok(())
            }
            ActiveFrameMutInner::Native(native) => native
                .registers
                .write(usize::from(register), value)
                .then_some(())
                .ok_or(VmError::InvalidOperand),
        }
    }

    /// Running function object's exact SELF value.
    #[must_use]
    pub fn self_value(&self) -> Value {
        match &self.inner {
            ActiveFrameMutInner::Materialized { frame, .. } => frame.self_value,
            // SAFETY: one scalar read under the native-view contract.
            ActiveFrameMutInner::Native(native) => unsafe { native.frame.as_ref().self_value() },
        }
    }

    /// Replace the running function object's SELF value.
    pub fn set_self_value(&mut self, value: Value) {
        match &mut self.inner {
            ActiveFrameMutInner::Materialized { frame, .. } => frame.self_value = value,
            // SAFETY: one scalar write under exclusive logical mutator access.
            ActiveFrameMutInner::Native(native) => unsafe {
                native.frame.as_mut().set_self_value(value);
            },
        }
    }

    /// Current `this` binding.
    #[must_use]
    pub fn this_value(&self) -> Value {
        match &self.inner {
            ActiveFrameMutInner::Materialized { frame, .. } => frame.this_value,
            // SAFETY: one scalar read under the native-view contract.
            ActiveFrameMutInner::Native(native) => unsafe { native.frame.as_ref().this_value() },
        }
    }

    /// Replace the current `this` binding.
    pub fn set_this_value(&mut self, value: Value) {
        match &mut self.inner {
            ActiveFrameMutInner::Materialized { frame, .. } => frame.this_value = value,
            // SAFETY: one scalar write under exclusive logical mutator access.
            ActiveFrameMutInner::Native(native) => unsafe {
                native.frame.as_mut().set_this_value(value);
            },
        }
    }

    /// Current `new.target` binding.
    #[must_use]
    pub fn new_target_value(&self) -> Value {
        match &self.inner {
            ActiveFrameMutInner::Materialized { new_target, .. } => *new_target,
            // SAFETY: one scalar read under the native-view contract.
            ActiveFrameMutInner::Native(native) => unsafe { native.frame.as_ref().new_target() },
        }
    }

    /// The context the running closure was created over; see
    /// [`ActiveFrameRef::closure_context`].
    #[must_use]
    pub fn closure_context(&self, heap: &otter_gc::GcHeap) -> Value {
        closure_context_of(self.self_value(), heap)
    }

    /// Enter interpreter dispatch over this same canonical activation.
    ///
    /// The native register window is retained verbatim. A
    /// materialized frame is already interpreter-owned and only normalizes its
    /// tier marker.
    pub fn enter_interpreter(&mut self) -> Result<(), VmError> {
        match &mut self.inner {
            ActiveFrameMutInner::Materialized { frame, .. } => {
                frame.header.kind = NativeFrameKind::Interpreter;
                Ok(())
            }
            // SAFETY: one non-safepointing scalar state transition.
            ActiveFrameMutInner::Native(native) => unsafe {
                native
                    .frame
                    .as_mut()
                    .enter_interpreter()
                    .then_some(())
                    .ok_or(VmError::InvalidOperand)
            },
        }
    }

    /// Enter a compiled tier over this same canonical native activation.
    ///
    /// Materialized interpreter activations enter compiled code through the
    /// JIT entry path, which constructs and publishes a native frame. This
    /// borrowed materialized view cannot perform that ownership transition.
    pub fn enter_compiled(&mut self, kind: NativeFrameKind) -> Result<(), VmError> {
        match &mut self.inner {
            ActiveFrameMutInner::Materialized { .. } => Err(VmError::InvalidOperand),
            // SAFETY: one non-safepointing scalar state transition.
            ActiveFrameMutInner::Native(native) => unsafe {
                native
                    .frame
                    .as_mut()
                    .enter_compiled(kind)
                    .then_some(())
                    .ok_or(VmError::InvalidOperand)
            },
        }
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

fn validate_frame_pointer(frame: *const NativeFrame) -> Result<(), ActiveFrameError> {
    if frame.is_null() {
        return Err(ActiveFrameError::NullNativeFrame);
    }
    if !(frame as usize).is_multiple_of(mem::align_of::<NativeFrame>()) {
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
        Frame {
            header: header(slots.len() as u16),
            registers: crate::RegisterWindow::attached(slots.as_mut_ptr(), slots.len(), 0),
            self_value: Value::function(7),
            this_value: Value::number_i32(9),
            return_register: None,
            cold: None,
        }
    }

    #[test]
    fn native_window_access_is_slot_scoped_across_gc_relocation() {
        let mut native_slots = [Value::number_i32(1), Value::undefined()];
        let native_base = native_slots.as_mut_ptr();
        let mut native = NativeFrame::new(
            header(native_slots.len() as u16),
            native_base as u64,
            Value::function(7),
            Value::number_i32(9),
        );
        {
            // SAFETY: `native` and `native_slots` remain exclusively live for
            // the view and match the published descriptors above.
            let mut active = unsafe { ActiveFrameMut::from_native_ptr(&mut native) }.unwrap();
            assert_eq!(active.storage(), ActiveFrameStorage::Native);
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
        assert_eq!(native.register_base, native_base as u64);
    }

    #[test]
    fn incoming_argument_window_follows_the_registers_and_is_traced() {
        let mut slots = [
            Value::number_i32(1),
            Value::number_i32(2),
            Value::number_i32(30),
            Value::number_i32(40),
            Value::number_i32(50),
        ];
        let mut native = NativeFrame::new(
            header(2),
            slots.as_mut_ptr() as u64,
            Value::function(7),
            Value::undefined(),
        );
        native.set_stack_registers();
        native.set_incoming_arguments(3);
        // SAFETY: the frame, its two registers, and the three published
        // arguments remain live for the view.
        let active = unsafe { ActiveFrameRef::from_native_ptr(&native) }.unwrap();
        assert_eq!(active.register_count(), 2);
        assert_eq!(active.incoming_argument_count(), Some(3));
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

        let mut plain = NativeFrame::new(
            header(2),
            slots.as_mut_ptr() as u64,
            Value::function(7),
            Value::undefined(),
        );
        plain.header.flags = NativeFrameFlags::from_bits(NativeFrameFlags::INCOMING_ARGUMENTS);
        plain.argument_count = 3;
        // SAFETY: the frame record is valid; the unowned argument window is
        // rejected before any window is formed.
        let unowned = unsafe { ActiveFrameRef::from_native_ptr(&plain) };
        assert!(matches!(
            unowned,
            Err(ActiveFrameError::UnownedIncomingArguments)
        ));
    }

    #[test]
    fn native_pointer_validation_rejects_invalid_descriptors() {
        // SAFETY: constructor validates null before dereferencing it.
        let null = unsafe { ActiveFrameMut::from_native_ptr(std::ptr::null_mut()) };
        assert!(matches!(null, Err(ActiveFrameError::NullNativeFrame)));

        let mut native = NativeFrame::new(header(1), 0, Value::function(7), Value::undefined());
        // SAFETY: the frame record is valid; its deliberately missing register
        // window is rejected before a slice is formed.
        let missing = unsafe { ActiveFrameMut::from_native_ptr(&mut native) };
        assert!(matches!(
            missing,
            Err(ActiveFrameError::MissingRegisterWindow)
        ));
    }

    #[test]
    fn native_single_slot_access_is_bounds_checked() {
        let mut slots = [Value::undefined()];
        let mut native = NativeFrame::new(
            header(slots.len() as u16),
            slots.as_mut_ptr() as u64,
            Value::function(7),
            Value::undefined(),
        );
        // SAFETY: frame and its one initialized slot remain live for the view.
        let mut active = unsafe { ActiveFrameMut::from_native_ptr(&mut native) }.unwrap();
        assert!(matches!(active.read(1), Err(VmError::InvalidOperand)));
        assert!(matches!(
            active.write(1, Value::undefined()),
            Err(VmError::InvalidOperand)
        ));
    }

    #[test]
    fn native_arguments_cache_is_rewritten_in_place() {
        let mut slots = [Value::undefined()];
        let mut native = NativeFrame::new(
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
        let active = unsafe { ActiveFrameRef::from_native_ptr(&native) }.unwrap();
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
    fn closure_context_reads_self_on_both_representations() {
        let mut heap = otter_gc::GcHeap::new().expect("heap");
        let context = crate::context::alloc_context_with_roots(
            &mut heap,
            crate::context::ContextShape {
                scope_function_id: 7,
                scope_index: 0,
                slot_count: 1,
                has_extension: false,
            },
            Value::undefined(),
            |_| false,
            &mut |_| {},
        )
        .expect("context");
        let closure =
            crate::closure::alloc_closure(&mut heap, 7, Value::context(context), None, None)
                .expect("closure");
        let mut slots = [Value::undefined()];
        let mut native = NativeFrame::new(
            header(1),
            slots.as_mut_ptr() as u64,
            Value::closure(closure),
            Value::undefined(),
        );
        // SAFETY: the frame and its window remain live for the view.
        let active = unsafe { ActiveFrameRef::from_native_ptr(&native) }.unwrap();
        assert_eq!(active.closure_context(&heap), Value::context(context));
        native.set_self_value(Value::function(7));
        // SAFETY: as above.
        let bare = unsafe { ActiveFrameRef::from_native_ptr(&native) }.unwrap();
        assert!(bare.closure_context(&heap).is_undefined());

        let mut frame_slots = [Value::undefined()];
        let mut frame = materialized_frame(&mut frame_slots);
        frame.self_value = Value::closure(closure);
        assert_eq!(
            ActiveFrameRef::materialized(&frame).closure_context(&heap),
            Value::context(context)
        );
    }

    #[test]
    fn parked_frames_keep_self_and_registers_across_park_and_resume() {
        let mut slots = [Value::number_i32(5)];
        let mut frame = materialized_frame(&mut slots);
        frame.self_value = Value::function(41);
        let (parked, window) = crate::frame_state::ParkedFrameState::copy_from_active(frame);
        let restored = parked.into_active(window);
        assert_eq!(restored.self_value, Value::function(41));
        assert_eq!(restored.registers[0], Value::number_i32(5));
    }
}
