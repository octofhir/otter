//! Native activation register-window descriptor.
//!
//! # Contents
//! - [`RegisterWindow`] exposes the tagged register extent in one [`crate::Frame`].
//! - Field offsets shared by both generated backends.
//!
//! # Invariants
//! The common trampoline owns storage on the native stack. A descriptor never
//! owns, reallocates, or releases its slots. Its initialized extent remains
//! stable until execution returns through the trampoline.
//!
//! # See also
//! - [`crate::native_abi::call_trampoline`]
//! - [`crate::active_frame`]

use crate::Value;

/// C-layout descriptor for one contiguous tagged register window.
#[repr(C, align(8))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegisterWindow {
    base: *mut Value,
    len: u32,
    reserved: u32,
}

impl RegisterWindow {
    pub(crate) fn attached(base: *mut Value, len: usize) -> Self {
        Self {
            base,
            len: u32::try_from(len).expect("register window length exceeds u32"),
            reserved: 0,
        }
    }

    /// Base of initialized tagged slots.
    #[must_use]
    pub fn as_mut_ptr(self) -> *mut Value {
        self.base
    }

    /// Number of initialized tagged slots.
    #[must_use]
    pub const fn len(self) -> usize {
        self.len as usize
    }

    /// Whether the window contains no tagged slots.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.len == 0
    }
}

impl std::ops::Deref for RegisterWindow {
    type Target = [Value];

    #[inline]
    fn deref(&self) -> &Self::Target {
        // SAFETY: the native trampoline retains the initialized extent for
        // the complete activation lifetime.
        unsafe { std::slice::from_raw_parts(self.base, self.len()) }
    }
}

impl std::ops::DerefMut for RegisterWindow {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: Frame owns exclusive mutable access to its attached window.
        unsafe { std::slice::from_raw_parts_mut(self.base, self.len()) }
    }
}

const _: [(); 16] = [(); std::mem::size_of::<RegisterWindow>()];
const _: [(); 8] = [(); std::mem::align_of::<RegisterWindow>()];
const _: [(); 0] = [(); std::mem::offset_of!(RegisterWindow, base)];
const _: [(); 8] = [(); std::mem::offset_of!(RegisterWindow, len)];
const _: [(); 12] = [(); std::mem::offset_of!(RegisterWindow, reserved)];
