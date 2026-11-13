//! Cage-address rebasing during exclusive startup-image relocation.
//!
//! # Contents
//! - A scoped capture-process cage base for the image's trace walk.
//! - Exact prefix validation and rebasing of full-width cage addresses.
//!
//! # Invariants
//! - Only heap-image restoration can enter the scope; no mutator or collecting
//!   operation runs inside it, and unwinding restores the previous context.
//! - Ordinary tracing never repairs a foreign address.
//! - Rebasing preserves the compressed offset for the existing page relocator.

use std::cell::Cell;

thread_local! {
    static SOURCE_CAGE: Cell<u64> = const { Cell::new(0) };
}

pub(crate) struct ImageCageScope(u64, std::marker::PhantomData<*const ()>);

impl ImageCageScope {
    pub(crate) fn enter(source: u64) -> Self {
        Self(
            SOURCE_CAGE.with(|current| current.replace(source)),
            std::marker::PhantomData,
        )
    }
}

impl Drop for ImageCageScope {
    fn drop(&mut self) {
        SOURCE_CAGE.with(|current| current.set(self.0));
    }
}

/// Rebase a full cage address only while restoring an image that captured it.
///
/// Call only for an address whose prefix differs from the current cage. Outside
/// image relocation it is returned unchanged, preserving ordinary trace checks.
#[doc(hidden)]
#[cold]
pub fn rebase_image_address(address: u64) -> u64 {
    SOURCE_CAGE.with(|source| {
        let source = source.get();
        if source == 0 {
            return address;
        }
        assert_eq!(
            address & !u64::from(u32::MAX),
            source,
            "restored address does not belong to the captured cage"
        );
        crate::cage_base() as u64 | (address & u64::from(u32::MAX))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_active_capture_prefix_is_rebased_and_offsets_stay_exact() {
        let _lock = crate::compressed::CAGE_TEST_LOCK.lock().unwrap();
        let _heap = crate::GcHeap::new().unwrap();
        let current = crate::cage_base() as u64;
        let source = current + (1 << 32);
        let address = source | 0x4_0040;
        assert_eq!(rebase_image_address(address), address);
        {
            let _scope = ImageCageScope::enter(source);
            assert_eq!(rebase_image_address(address), current | 0x4_0040);
        }
        assert_eq!(rebase_image_address(address), address);
    }

    #[test]
    fn an_unrelated_prefix_is_rejected_and_unwinding_clears_the_scope() {
        let _lock = crate::compressed::CAGE_TEST_LOCK.lock().unwrap();
        let _heap = crate::GcHeap::new().unwrap();
        let source = crate::cage_base() as u64 + (1 << 32);
        let address = source | 0x4_0040;
        let result = std::panic::catch_unwind(|| {
            let _scope = ImageCageScope::enter(source);
            rebase_image_address((source + (1 << 32)) | 0x4_0040);
        });
        assert!(result.is_err());
        assert_eq!(rebase_image_address(address), address);
    }
}
