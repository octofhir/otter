//! Default-off scalar sizes of actual native call-entry instruction spans.
//!
//! # Contents
//! - Exact assembler coordinates for selectors, publication and reservation.
//! - Owned byte counts for tests and explicitly requested inspector evidence.
//!
//! # Invariants
//! - Labels add no machine instructions, calls, stores or runtime clock reads.
//! - On Mach-O every interior label is an alternate entry of its function atom,
//!   so default-off capture cannot change instruction retention or branch layout.
//! - Returned values contain neither executable addresses nor VM handles.
//! - One canonical Host kernel serves Generic and Native entry. Static byte
//!   spans are layout evidence; they are never an elapsed performance claim.
//! - The shared reservation span includes both branch outcomes and preparation
//!   code. Native selection skips preparation through the existing zero flag.
//!
//! # See also
//! - [`super::call_request_entry`] publishes the sole pending request.
//! - `otter-jit::entry::native_kind_tests` measures exact caller emission.

unsafe extern "C" {
    #[link_name = "otter_native_generic_end"]
    pub(super) static GENERIC_END: u8;
    #[link_name = "otter_native_selected_end"]
    pub(super) static NATIVE_END: u8;
    #[link_name = "otter_native_publisher_end"]
    pub(super) static PUBLISHER_END: u8;
    #[link_name = "otter_native_header_start"]
    pub(super) static HEADER_START: u8;
    #[link_name = "otter_native_header_end"]
    pub(super) static HEADER_END: u8;
    #[link_name = "otter_native_classifier"]
    pub(super) static CLASSIFIER: u8;
    #[link_name = "otter_native_reservation"]
    pub(super) static RESERVATION: u8;
    #[link_name = "otter_native_invoke"]
    pub(super) static INVOKE: u8;
    #[link_name = "otter_native_trampoline_end"]
    pub(super) static TRAMPOLINE_END: u8;
}

/// Actual instruction-span bytes in this compiled VM binary.
#[doc(hidden)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NativeEntryCodeSizes {
    /// Ordinary callable selector before the common publisher.
    pub generic_selector_bytes: u32,
    /// Proved native-kind selector before that same publisher.
    pub native_selector_bytes: u32,
    /// Shared complete request publication and platform C entry.
    pub request_publisher_bytes: u32,
    /// Complete Host header materialization traversed only by Native selection.
    pub native_header_bytes: u32,
    /// Shared trampoline including admission, roots, entry and completion.
    pub trampoline_bytes: u32,
    /// General classification region skipped by a selected Native request.
    pub classifier_bytes: u32,
    /// Shared admission, copy, publication and optional preparation region.
    pub reservation_bytes: u32,
}

/// Capture scalar layout evidence only when explicitly requested.
#[doc(hidden)]
#[must_use]
pub fn native_entry_code_sizes() -> NativeEntryCodeSizes {
    fn span(start: usize, end: usize) -> u32 {
        u32::try_from(end.checked_sub(start).expect("ordered assembler span"))
            .expect("native entry span fits a scalar byte count")
    }
    // These are addresses of linker-owned labels inside the retained native
    // text functions. Taking their addresses reads no memory; no address escapes.
    NativeEntryCodeSizes {
        generic_selector_bytes: span(
            super::call_generic_entry as *const () as usize,
            std::ptr::addr_of!(GENERIC_END).addr(),
        ),
        native_selector_bytes: span(
            super::call_native_entry as *const () as usize,
            std::ptr::addr_of!(NATIVE_END).addr(),
        ),
        request_publisher_bytes: span(
            super::call_request_entry as *const () as usize,
            std::ptr::addr_of!(PUBLISHER_END).addr(),
        ),
        native_header_bytes: span(
            std::ptr::addr_of!(HEADER_START).addr(),
            std::ptr::addr_of!(HEADER_END).addr(),
        ),
        trampoline_bytes: span(
            super::call_trampoline as *const () as usize,
            std::ptr::addr_of!(TRAMPOLINE_END).addr(),
        ),
        classifier_bytes: span(
            std::ptr::addr_of!(CLASSIFIER).addr(),
            std::ptr::addr_of!(RESERVATION).addr(),
        ),
        reservation_bytes: span(
            std::ptr::addr_of!(RESERVATION).addr(),
            std::ptr::addr_of!(INVOKE).addr(),
        ),
    }
}

#[cfg(test)]
mod tests;
