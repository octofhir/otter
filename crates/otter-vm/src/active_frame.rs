//! Representation-neutral access to a live JavaScript activation.
//!
//! [`NativeFrame`] plus its published register/upvalue windows is the canonical
//! activation shared by interpreter, baseline, and optimizing tiers. Tier
//! switches mutate only execution metadata and preserve those windows. A
//! materialized [`Frame`] is supported as a legacy/cold-sidecar adapter for
//! paths that have not moved to the canonical activation yet. Runtime semantics
//! should not otherwise know which representation they received.
//!
//! # Contents
//! - [`ActiveFrameRef`] — shared access to common frame state.
//! - [`ActiveFrameMut`] — register, binding, PC, and frame-state mutation.
//! - [`ActiveFrameStorage`] — the active physical representation.
//! - [`ActiveFrameError`] — validation failures at the native ABI boundary.
//!
//! # Invariants
//! - Native views are created at one audited `unsafe` boundary. Their register
//!   and upvalue descriptors must refer to published, initialized storage for
//!   the whole view lifetime.
//! - Native windows stay raw inside the view. Safe operations create no slice
//!   whose borrow can survive an allocating or reentrant VM call; reads return
//!   copied handles and writes touch exactly one checked slot.
//! - Common operations never expose the `Vec`/`Box` layout that owns an
//!   interpreter frame's upvalue spine.
//! - Interpreter entry on a native activation is zero-copy: register base,
//!   upvalue base, SELF, and `this` remain authoritative in [`NativeFrame`].
//! - A mutable native view has exclusive logical ownership of its frame record
//!   and tagged windows. It deliberately does not manufacture long-lived Rust
//!   references to register-stack storage: GC and reentrant runtime work may
//!   revisit that storage through the owning interpreter between operations.
//!   Upvalue value writes still flow through the GC write-barrier API.
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
    Frame, UpvalueCell, Value, VmError,
    native_abi::{NativeFrame, NativeFrameKind, VmFrameHeader},
};

/// Physical representation backing an active frame view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveFrameStorage {
    /// Full interpreter [`Frame`] published on a `HoltStack`.
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
    /// A non-empty upvalue spine had no base address.
    MissingUpvalueSpine,
    /// The upvalue-spine base was not aligned for [`UpvalueCell`].
    MisalignedUpvalueSpine,
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
            Self::MissingUpvalueSpine => "non-empty native upvalue spine has no base",
            Self::MisalignedUpvalueSpine => "native upvalue spine is misaligned",
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
struct NativeFrameRef<'a> {
    frame: &'a NativeFrame,
    registers: NativeWindow<Value>,
    upvalues: NativeWindow<UpvalueCell>,
}

#[derive(Debug)]
struct NativeFrameMut<'a> {
    frame: &'a mut NativeFrame,
    registers: NativeWindow<Value>,
    upvalues: NativeWindow<UpvalueCell>,
}

#[derive(Debug)]
enum ActiveFrameRefInner<'a> {
    Materialized { frame: &'a Frame, new_target: Value },
    Native(NativeFrameRef<'a>),
}

#[derive(Debug)]
enum ActiveFrameMutInner<'a> {
    Materialized {
        frame: &'a mut Frame,
        new_target: Value,
    },
    Native(NativeFrameMut<'a>),
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
    /// `frame` must remain valid and immutable for `'a`. Its non-empty
    /// register and upvalue descriptors must point to initialized storage that
    /// remains live for `'a`; their backing allocations must not move. The
    /// boxed value fields must contain valid [`Value`] bit patterns.
    pub unsafe fn from_native_ptr(frame: *const NativeFrame) -> Result<Self, ActiveFrameError> {
        validate_frame_pointer(frame)?;
        // SAFETY: upheld by the caller after the null/alignment validation.
        let frame = unsafe { &*frame };
        let registers = checked_window::<Value>(
            frame.register_base,
            usize::from(frame.header.register_count),
            ActiveFrameError::MissingRegisterWindow,
            ActiveFrameError::MisalignedRegisterWindow,
        )?;
        let upvalues = checked_window::<UpvalueCell>(
            frame.upvalue_base,
            frame.upvalue_count as usize,
            ActiveFrameError::MissingUpvalueSpine,
            ActiveFrameError::MisalignedUpvalueSpine,
        )?;
        Ok(Self {
            inner: ActiveFrameRefInner::Native(NativeFrameRef {
                frame,
                registers,
                upvalues,
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
    pub fn header(&self) -> &VmFrameHeader {
        match &self.inner {
            ActiveFrameRefInner::Materialized { frame, .. } => &frame.header,
            ActiveFrameRefInner::Native(native) => &native.frame.header,
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

    /// Running function object's exact SELF value.
    #[must_use]
    pub fn self_value(&self) -> Value {
        match &self.inner {
            ActiveFrameRefInner::Materialized { frame, .. } => frame.self_value,
            ActiveFrameRefInner::Native(native) => native.frame.self_value(),
        }
    }

    /// Current `this` binding.
    #[must_use]
    pub fn this_value(&self) -> Value {
        match &self.inner {
            ActiveFrameRefInner::Materialized { frame, .. } => frame.this_value,
            ActiveFrameRefInner::Native(native) => native.frame.this_value(),
        }
    }

    /// Current `new.target` binding.
    #[must_use]
    pub fn new_target_value(&self) -> Value {
        match &self.inner {
            ActiveFrameRefInner::Materialized { new_target, .. } => *new_target,
            ActiveFrameRefInner::Native(native) => native.frame.new_target(),
        }
    }

    /// Number of captured upvalue handles in this activation.
    #[must_use]
    pub fn upvalue_count(&self) -> usize {
        match &self.inner {
            ActiveFrameRefInner::Materialized { frame, .. } => frame.upvalues.len(),
            ActiveFrameRefInner::Native(native) => native.upvalues.len,
        }
    }

    /// Raw base of the initialized upvalue-handle spine.
    ///
    /// This exists for native entry construction. Runtime semantics should use
    /// [`Self::upvalue`] so no borrowed slice survives a VM transition.
    #[must_use]
    pub fn upvalue_base_ptr(&self) -> *const UpvalueCell {
        match &self.inner {
            ActiveFrameRefInner::Materialized { frame, .. } => frame.upvalues.as_ptr(),
            ActiveFrameRefInner::Native(native) => native.upvalues.base.as_ptr().cast_const(),
        }
    }

    /// Read one captured upvalue handle.
    pub fn upvalue(&self, index: u32) -> Result<UpvalueCell, VmError> {
        match &self.inner {
            ActiveFrameRefInner::Materialized { frame, .. } => frame
                .upvalues
                .get(index as usize)
                .copied()
                .ok_or(VmError::InvalidOperand),
            ActiveFrameRefInner::Native(native) => native
                .upvalues
                .read(index as usize)
                .ok_or(VmError::InvalidOperand),
        }
    }

    /// Trace non-register GC slots owned by this activation.
    ///
    /// Register windows are traced once through the published register-stack
    /// prefix. This method owns SELF, `this`, `new.target`, and upvalue handles
    /// for a native activation; a legacy materialized adapter delegates to its
    /// established frame tracer.
    pub(crate) fn trace_non_register_slots(&self, visitor: &mut SlotVisitor<'_>) {
        match &self.inner {
            ActiveFrameRefInner::Materialized { frame, .. } => frame.trace_frame_slots(visitor),
            ActiveFrameRefInner::Native(native) => {
                for bits in [
                    &native.frame.self_value_bits,
                    &native.frame.this_value_bits,
                    &native.frame.new_target_bits,
                ] {
                    // SAFETY: Value is transparent over `u64`; native-frame
                    // publication guarantees valid boxed bits and stop-the-
                    // world root tracing owns in-place relocation updates.
                    unsafe {
                        (&*(std::ptr::from_ref(bits).cast::<Value>())).trace_value_slots(visitor)
                    };
                }
                for index in 0..native.upvalues.len {
                    // SAFETY: the validated published spine remains live for
                    // this activation; no Rust slice/reference is retained.
                    let slot = unsafe { native.upvalues.base.as_ptr().add(index) };
                    visitor(slot.cast::<RawGc>());
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
    /// `frame` must remain valid and exclusively borrowed for `'a`. Its
    /// register and upvalue descriptors must point to initialized, stable
    /// storage with exclusive logical mutator ownership for `'a`. The owning
    /// VM may inspect or relocate published slots during GC, but no independent
    /// semantic writer may race this activation. No owner may reclaim or move
    /// either window until the returned view is dropped. Boxed fields must
    /// contain valid [`Value`] bit patterns.
    pub unsafe fn from_native_ptr(frame: *mut NativeFrame) -> Result<Self, ActiveFrameError> {
        validate_frame_pointer(frame.cast_const())?;
        // SAFETY: upheld by the caller after the null/alignment validation.
        let frame = unsafe { &mut *frame };
        let registers = checked_window::<Value>(
            frame.register_base,
            usize::from(frame.header.register_count),
            ActiveFrameError::MissingRegisterWindow,
            ActiveFrameError::MisalignedRegisterWindow,
        )?;
        let upvalues = checked_window::<UpvalueCell>(
            frame.upvalue_base,
            frame.upvalue_count as usize,
            ActiveFrameError::MissingUpvalueSpine,
            ActiveFrameError::MisalignedUpvalueSpine,
        )?;
        Ok(Self {
            inner: ActiveFrameMutInner::Native(NativeFrameMut {
                frame,
                registers,
                upvalues,
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
                    upvalues: native.upvalues,
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
    pub fn header(&self) -> &VmFrameHeader {
        match &self.inner {
            ActiveFrameMutInner::Materialized { frame, .. } => &frame.header,
            ActiveFrameMutInner::Native(native) => &native.frame.header,
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
            ActiveFrameMutInner::Native(native) => native.frame.header.pc = pc,
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
            ActiveFrameMutInner::Native(native) => native.frame.self_value(),
        }
    }

    /// Replace the running function object's SELF value.
    pub fn set_self_value(&mut self, value: Value) {
        match &mut self.inner {
            ActiveFrameMutInner::Materialized { frame, .. } => frame.self_value = value,
            ActiveFrameMutInner::Native(native) => native.frame.set_self_value(value),
        }
    }

    /// Current `this` binding.
    #[must_use]
    pub fn this_value(&self) -> Value {
        match &self.inner {
            ActiveFrameMutInner::Materialized { frame, .. } => frame.this_value,
            ActiveFrameMutInner::Native(native) => native.frame.this_value(),
        }
    }

    /// Replace the current `this` binding.
    pub fn set_this_value(&mut self, value: Value) {
        match &mut self.inner {
            ActiveFrameMutInner::Materialized { frame, .. } => frame.this_value = value,
            ActiveFrameMutInner::Native(native) => native.frame.set_this_value(value),
        }
    }

    /// Current `new.target` binding.
    #[must_use]
    pub fn new_target_value(&self) -> Value {
        match &self.inner {
            ActiveFrameMutInner::Materialized { new_target, .. } => *new_target,
            ActiveFrameMutInner::Native(native) => native.frame.new_target(),
        }
    }

    /// Number of captured upvalue handles in this activation.
    #[must_use]
    pub fn upvalue_count(&self) -> usize {
        match &self.inner {
            ActiveFrameMutInner::Materialized { frame, .. } => frame.upvalues.len(),
            ActiveFrameMutInner::Native(native) => native.upvalues.len,
        }
    }

    /// Raw base of the initialized upvalue-handle spine.
    ///
    /// Native entry construction consumes this descriptor. Runtime semantics
    /// should use [`Self::upvalue`] and [`Self::replace_upvalue`].
    #[must_use]
    pub fn upvalue_base_ptr(&self) -> *mut UpvalueCell {
        match &self.inner {
            ActiveFrameMutInner::Materialized { frame, .. } => frame.upvalues.as_ptr().cast_mut(),
            ActiveFrameMutInner::Native(native) => native.upvalues.base.as_ptr(),
        }
    }

    /// Read one captured upvalue handle.
    pub fn upvalue(&self, index: u32) -> Result<UpvalueCell, VmError> {
        match &self.inner {
            ActiveFrameMutInner::Materialized { frame, .. } => frame
                .upvalues
                .get(index as usize)
                .copied()
                .ok_or(VmError::InvalidOperand),
            ActiveFrameMutInner::Native(native) => native
                .upvalues
                .read(index as usize)
                .ok_or(VmError::InvalidOperand),
        }
    }

    /// Replace one activation-local captured-cell handle.
    pub fn replace_upvalue(&mut self, index: u32, cell: UpvalueCell) -> Result<(), VmError> {
        match &mut self.inner {
            ActiveFrameMutInner::Materialized { frame, .. } => {
                let slot = frame
                    .upvalues
                    .get_mut(index as usize)
                    .ok_or(VmError::InvalidOperand)?;
                *slot = cell;
                Ok(())
            }
            ActiveFrameMutInner::Native(native) => native
                .upvalues
                .write(index as usize, cell)
                .then_some(())
                .ok_or(VmError::InvalidOperand),
        }
    }

    /// Enter interpreter dispatch over this same canonical activation.
    ///
    /// Native register and upvalue windows are retained verbatim. A
    /// materialized frame is already interpreter-owned and only normalizes its
    /// tier marker.
    pub fn enter_interpreter(&mut self) -> Result<(), VmError> {
        match &mut self.inner {
            ActiveFrameMutInner::Materialized { frame, .. } => {
                frame.header.kind = NativeFrameKind::Interpreter;
                Ok(())
            }
            ActiveFrameMutInner::Native(native) => native
                .frame
                .enter_interpreter()
                .then_some(())
                .ok_or(VmError::InvalidOperand),
        }
    }

    /// Enter a compiled tier over this same canonical native activation.
    ///
    /// Materialized interpreter activations enter compiled code through the
    /// JIT entry adapter, which constructs and publishes a native frame. This
    /// borrowed materialized view cannot perform that ownership transition.
    pub fn enter_compiled(&mut self, kind: NativeFrameKind) -> Result<(), VmError> {
        match &mut self.inner {
            ActiveFrameMutInner::Materialized { .. } => Err(VmError::InvalidOperand),
            ActiveFrameMutInner::Native(native) => native
                .frame
                .enter_compiled(kind)
                .then_some(())
                .ok_or(VmError::InvalidOperand),
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
        return Ok(NativeWindow {
            base: NonNull::dangling(),
            len: 0,
        });
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
            code_block_id: 11,
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
            upvalues: Frame::empty_upvalues(),
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
        native.set_materialized_activation(0);
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
    fn materialized_self_survives_park_and_resume() {
        let mut slots = [Value::number_i32(5)];
        let mut frame = materialized_frame(&mut slots);
        frame.self_value = Value::function(41);
        let (parked, window) = crate::frame_state::ParkedFrameState::copy_from_active(frame);
        let restored = parked.into_active(window);
        assert_eq!(restored.self_value, Value::function(41));
        assert_eq!(restored.registers[0], Value::number_i32(5));
    }
}
