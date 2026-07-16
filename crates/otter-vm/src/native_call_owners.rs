//! VM ownership for frameless compiled-call resources.
//!
//! # Contents
//! - [`NativeCallOwner`] keeps one compiled callee's register window and
//!   upvalue storage alive without constructing an interpreter [`crate::Frame`].
//! - [`NativeCallUpvalues`] borrows a closure spine on the inherited-only hot
//!   path and owns a box only when the callee creates fresh cells.
//! - [`NativeCallOwnerStack`] publishes and removes owners in exact LIFO order.
//!
//! # Invariants
//! - An owner is pushed only after every allocating part of call preparation
//!   succeeds. There is no GC boundary between owner publication and native
//!   frame publication by generated code.
//! - [`crate::register_stack::RegisterStack`] traces every owned register
//!   window. While compiled code is live, its published
//!   [`crate::NativeFrame`] traces the upvalue source and exact closure SELF.
//!   That SELF owns the backing allocation of every borrowed closure spine.
//! - Removing an owner requires its exact id to be at the top. A mismatched id
//!   never releases a younger activation or raises the register-stack cursor.
//! - A hot return or abort drops the owner directly. Only a fired bailout may
//!   move its resources into one cold materialized [`crate::Frame`].
//!
//! # See also
//! - [`crate::interp::jit_call`] for direct-call prepare and completion.
//! - [`crate::RegisterWindow`] for reservation-stable register ownership.

use std::mem::ManuallyDrop;

use crate::{
    RegisterWindow, UpvalueCell, VmError, frame_state::UpvalueSpine, native_abi::NativeFrameKind,
    upvalue_source::UpvalueSource,
};

/// Ownership-aware native upvalue storage in the same 16 bytes as a boxed
/// slice. The low pointer bit distinguishes an owned box from a borrowed stable
/// closure allocation; [`UpvalueCell`] alignment leaves that bit available.
#[derive(Debug)]
pub(crate) struct NativeCallUpvalues {
    tagged_base: usize,
    len: u32,
}

impl NativeCallUpvalues {
    const OWNED_TAG: usize = 1;

    /// Borrow a stable source. The exact closure stored in the published
    /// native frame keeps closure-backed sources alive.
    #[must_use]
    pub(crate) fn borrowed(source: UpvalueSource) -> Self {
        let base = source.base_ptr_or_null() as usize;
        debug_assert_eq!(base & Self::OWNED_TAG, 0);
        Self {
            tagged_base: base,
            len: source.len_u32(),
        }
    }

    /// Adopt an allocated spine without copying it.
    #[must_use]
    pub(crate) fn owned(spine: UpvalueSpine) -> Self {
        if spine.is_empty() {
            return Self::borrowed(UpvalueSource::empty());
        }
        let len = u32::try_from(spine.len()).expect("native upvalue spine exceeds u32");
        let raw = Box::into_raw(spine);
        let base = raw.cast::<UpvalueCell>() as usize;
        debug_assert_eq!(base & Self::OWNED_TAG, 0);
        Self {
            tagged_base: base | Self::OWNED_TAG,
            len,
        }
    }

    /// Current allocation-neutral source for frame publication and validation.
    #[must_use]
    pub(crate) fn source(&self) -> UpvalueSource {
        let base = (self.tagged_base & !Self::OWNED_TAG) as *mut UpvalueCell;
        // SAFETY: both constructors publish initialized stable storage for the
        // complete NativeCallUpvalues lifetime. Empty storage canonicalizes the
        // possibly-null base inside UpvalueSource.
        unsafe { UpvalueSource::from_raw_parts(base, self.len) }
            .expect("native owner must contain a valid upvalue source")
    }

    /// Whether this owner adopted an allocation instead of borrowing one.
    #[cfg(test)]
    #[must_use]
    pub(crate) const fn is_owned(&self) -> bool {
        self.tagged_base & Self::OWNED_TAG != 0
    }

    /// Move owned storage directly into a frame, or make the sole cold copy of
    /// a borrowed closure spine when bailout materialization actually fires.
    #[must_use]
    pub(crate) fn into_materialized(self) -> UpvalueSpine {
        let this = ManuallyDrop::new(self);
        if this.tagged_base & Self::OWNED_TAG == 0 {
            return this.source().copy_owned();
        }
        let base = (this.tagged_base & !Self::OWNED_TAG) as *mut UpvalueCell;
        let slice = std::ptr::slice_from_raw_parts_mut(base, this.len as usize);
        // SAFETY: `owned` obtained this exact pointer/length from Box::into_raw,
        // and ManuallyDrop prevents the regular destructor from reclaiming it.
        unsafe { Box::from_raw(slice) }
    }
}

impl Drop for NativeCallUpvalues {
    fn drop(&mut self) {
        if self.tagged_base & Self::OWNED_TAG == 0 {
            return;
        }
        let base = (self.tagged_base & !Self::OWNED_TAG) as *mut UpvalueCell;
        let slice = std::ptr::slice_from_raw_parts_mut(base, self.len as usize);
        // SAFETY: an owned record was created by Box::into_raw and this is its
        // unique unreclaimed drop path.
        unsafe { drop(Box::from_raw(slice)) };
    }
}

const _: () = assert!(std::mem::align_of::<UpvalueCell>() >= 2);
const _: [(); 16] = [(); std::mem::size_of::<NativeCallUpvalues>()];

/// Resources whose lifetime spans one frameless compiled callee.
#[derive(Debug)]
pub(crate) struct NativeCallOwner {
    /// Function identity retained for tiering feedback and bailout validation.
    pub(crate) function_id: u32,
    /// Selected compiled tier; occupies the former alignment padding.
    pub(crate) tier: NativeFrameKind,
    /// Stable register window rooted by the interpreter register stack.
    pub(crate) registers: RegisterWindow,
    /// Borrowed inherited cells or owned fresh+inherited cells.
    pub(crate) upvalues: NativeCallUpvalues,
}

/// LIFO owner arena for nested compiled-to-compiled calls.
#[derive(Debug, Default)]
pub(crate) struct NativeCallOwnerStack {
    owners: Vec<NativeCallOwner>,
}

impl NativeCallOwnerStack {
    /// Empty stack; capacity grows only to the native nesting depth observed.
    #[must_use]
    pub(crate) const fn new() -> Self {
        Self { owners: Vec::new() }
    }

    /// Publish one fully prepared owner and return its compact activation id.
    pub(crate) fn push(&mut self, owner: NativeCallOwner) -> Result<u32, VmError> {
        let owner_id = u32::try_from(self.owners.len()).map_err(|_| VmError::InvalidOperand)?;
        self.owners.push(owner);
        Ok(owner_id)
    }

    /// Remove exactly the youngest owner.
    pub(crate) fn pop(&mut self, owner_id: u32) -> Result<NativeCallOwner, VmError> {
        let expected = self
            .owners
            .len()
            .checked_sub(1)
            .and_then(|index| u32::try_from(index).ok());
        if expected != Some(owner_id) {
            return Err(VmError::InvalidOperand);
        }
        self.owners.pop().ok_or(VmError::InvalidOperand)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.owners.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Value;

    fn owner(function_id: u32, stack_base: u32) -> NativeCallOwner {
        NativeCallOwner {
            function_id,
            tier: NativeFrameKind::Baseline,
            registers: RegisterWindow::attached(std::ptr::null_mut::<Value>(), 0, stack_base),
            upvalues: NativeCallUpvalues::borrowed(UpvalueSource::empty()),
        }
    }

    #[test]
    fn owners_are_exact_lifo_without_preallocation() {
        let mut owners = NativeCallOwnerStack::new();
        assert_eq!(owners.len(), 0);
        assert_eq!(owners.owners.capacity(), 0);
        let outer = owners.push(owner(11, 0)).unwrap();
        let inner = owners.push(owner(22, 4)).unwrap();
        assert_eq!((outer, inner), (0, 1));

        assert!(matches!(owners.pop(outer), Err(VmError::InvalidOperand)));
        assert_eq!(owners.len(), 2);
        assert_eq!(owners.pop(inner).unwrap().function_id, 22);
        assert_eq!(owners.pop(outer).unwrap().function_id, 11);
        assert_eq!(owners.len(), 0);
    }

    #[test]
    fn borrowed_spine_allocates_only_when_materialized() {
        let mut heap = otter_gc::GcHeap::new().unwrap();
        let cells = vec![
            crate::alloc_upvalue(&mut heap, Value::undefined()).unwrap(),
            crate::alloc_upvalue(&mut heap, Value::null()).unwrap(),
        ];
        // SAFETY: the vector remains alive and is never resized while storage
        // borrows it.
        let source = unsafe { UpvalueSource::from_stable_slice(&cells) }.unwrap();
        let storage = NativeCallUpvalues::borrowed(source);
        assert!(!storage.is_owned());
        assert_eq!(
            storage.source().base_ptr_or_null(),
            cells.as_ptr().cast_mut()
        );

        let materialized = storage.into_materialized();
        assert_eq!(materialized.as_ref(), cells.as_slice());
        assert_ne!(materialized.as_ptr(), cells.as_ptr());
    }

    #[test]
    fn owned_spine_moves_without_copying_and_owner_stays_compact() {
        let mut heap = otter_gc::GcHeap::new().unwrap();
        let spine =
            vec![crate::alloc_upvalue(&mut heap, Value::undefined()).unwrap()].into_boxed_slice();
        let original = spine.as_ptr();
        let storage = NativeCallUpvalues::owned(spine);
        assert!(storage.is_owned());
        assert_eq!(storage.source().base_ptr_or_null(), original.cast_mut());
        let materialized = storage.into_materialized();
        assert_eq!(materialized.as_ptr(), original);
        assert_eq!(std::mem::size_of::<NativeCallOwner>(), 40);
    }
}
