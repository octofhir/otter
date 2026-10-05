//! Scalar inspection of cells retained by an active native handle scope.
//!
//! # Contents
//! - The actual managed footprint of an ordinary object cell.
//! - ECMAScript `typeof` through the canonical current-value classifier.
//!
//! # Invariants
//! - Every query resolves its current value from the collector-rewritten arena.
//! - Inspection cannot allocate a JS value, collect or reenter JavaScript.
//! - Only owned scalar facts leave the scope; no header pointer is exposed.
//!
//! # See also
//! - `super::NativeScope` owns rooted native operands.
//! - `crate::object::FieldLayout` describes the inline field region.

use super::{Local, NativeError, NativeScope};

impl NativeScope<'_, '_> {
    /// ECMAScript `typeof` classification of the current rooted value.
    ///
    /// This is the bytecode classifier, including null as Object and callable
    /// proxies as Function. It invokes no getter or JavaScript callback.
    #[must_use]
    pub fn typeof_kind(&self, value: Local<'_>) -> otter_bytecode::TypeOfKind {
        self.raw(value).typeof_kind_with_heap(self.ctx.heap())
    }

    /// Actual bytes of this ordinary object's managed cell, including its GC
    /// header and immutable inline prefix. Separate property slabs, exotic
    /// state and external storage are excluded. This query does not allocate
    /// a JavaScript value or invoke JavaScript.
    pub fn object_allocation_bytes(&self, value: Local<'_>) -> Result<usize, NativeError> {
        let object = self
            .raw(value)
            .as_object()
            .ok_or_else(|| NativeError::TypeError {
                name: "NativeScope::object_allocation_bytes",
                reason: "expected an ordinary object".to_owned(),
            })?;
        // SAFETY: the arena retains this exact current ordinary object. The
        // mutator owns the scope, and no allocation or reentry occurs between
        // resolving the handle and reading its authoritative header.
        Ok(unsafe { (*object.as_header_ptr()).size_bytes() as usize })
    }
}
