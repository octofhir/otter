//! Page-based generational tracing garbage collector for the Otter JS runtime.
//!
//! # Architecture
//!
//! Modeled after V8's Orinoco GC and JSC's Riptide:
//!
//! - **Page-based heap**: Memory organized into aligned 256 KB pages. Any object
//!   address can find its page header via bitmask: `addr & !(PAGE_SIZE - 1)`.
//! - **Generational**: Young generation (semi-space scavenger with bump allocation)
//!   and old generation (mark-sweep with free lists).
//! - **Typed headers**: Every GC object starts with an 8-byte [`GcHeader`] encoding
//!   type tag, size, and GC flags. No `dyn Any`, no downcasting on hot paths.
//! - **Marking bitmap**: 2 bits per cell stored in the page header, not in object
//!   headers. Enables O(1) mark reset via logical versioning.
//! - **Write barriers**: Combined generational + incremental marking barrier called
//!   on every pointer store.
//! - **Handle-based rooting**: `HandleScope`/`Local<T>` like V8, RAII-based.

pub mod barrier;
pub mod handle;
pub mod header;
pub mod heap;
pub mod marking;
pub mod page;
pub mod scavenger;
pub mod space;
pub mod trace;
pub mod typed;

/// Minimum object alignment (8 bytes, matching NaN-boxed value size).
pub const OBJECT_ALIGNMENT: usize = 8;

/// Align `size` up to the next multiple of [`OBJECT_ALIGNMENT`].
#[inline]
pub const fn align_up(size: usize, align: usize) -> usize {
    (size + align - 1) & !(align - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn align_up_basics() {
        assert_eq!(align_up(0, 8), 0);
        assert_eq!(align_up(1, 8), 8);
        assert_eq!(align_up(7, 8), 8);
        assert_eq!(align_up(8, 8), 8);
        assert_eq!(align_up(9, 8), 16);
        assert_eq!(align_up(24, 8), 24);
    }
}
