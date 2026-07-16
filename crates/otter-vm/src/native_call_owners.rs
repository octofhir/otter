//! VM ownership for frameless compiled-call resources.
//!
//! # Contents
//! - [`NativeCallOwner`] keeps one compiled callee's register window and
//!   upvalue spine alive without constructing an interpreter [`crate::Frame`].
//! - [`NativeCallOwnerStack`] publishes and removes owners in exact LIFO order.
//!
//! # Invariants
//! - An owner is pushed only after every allocating part of call preparation
//!   succeeds. There is no GC boundary between owner publication and native
//!   frame publication by generated code.
//! - [`crate::register_stack::RegisterStack`] traces every owned register
//!   window. While compiled code is live, its published
//!   [`crate::NativeFrame`] traces the upvalue spine and scalar bindings.
//! - Removing an owner requires its exact id to be at the top. A mismatched id
//!   never releases a younger activation or raises the register-stack cursor.
//! - A hot return or abort drops the owner directly. Only a fired bailout may
//!   move its resources into one cold materialized [`crate::Frame`].
//!
//! # See also
//! - [`crate::interp::jit_call`] for direct-call prepare and completion.
//! - [`crate::RegisterWindow`] for reservation-stable register ownership.

use crate::{RegisterWindow, VmError, frame_state::UpvalueSpine};

/// Resources whose lifetime spans one frameless compiled callee.
#[derive(Debug)]
pub(crate) struct NativeCallOwner {
    /// Function identity retained for tiering feedback and bailout validation.
    pub(crate) function_id: u32,
    /// Stable register window rooted by the interpreter register stack.
    pub(crate) registers: RegisterWindow,
    /// Stable upvalue-cell handle allocation referenced by the native frame.
    pub(crate) upvalues: UpvalueSpine,
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
            registers: RegisterWindow::attached(std::ptr::null_mut::<Value>(), 0, stack_base),
            upvalues: Vec::new().into_boxed_slice(),
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
}
