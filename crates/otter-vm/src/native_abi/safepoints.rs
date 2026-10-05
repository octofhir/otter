//! Code-object-owned safepoint and frame-map contracts.
//!
//! # Contents
//! - [`SafepointEntry`] associates an exact native return offset with one record.
//! - [`SafepointRecord`] is the VM-owned root map consumed by current native
//!   frames: the native spill roots beyond the canonical register window.
//!
//! # Invariants
//! - A JS child or pending request carries its caller's actual return address;
//!   the caller's code id resolves it in that exact retained generation.
//! - Active C helpers publish `(code_object_id, safepoint_id)` before collection.
//!   Neither boundary supplies a raw metadata-table pointer.
//! - The collector traces every published frame's canonical register window
//!   itself, so a record never names a window slot; it names exactly the live
//!   tagged native spill slots.
//! - Inline activation recipes use the same spill slots and code generation;
//!   they own no moving values or alternate runtime frame layout. A publication
//!   flag distinguishes generated native parents from runtime-published recipes.
//! - Machine-register roots are saved to mapped spill slots before an
//!   allocating or reentrant call.
//! - Owned backing-byte accounting includes reserved tagged-vector capacity
//!   and nested inline recipes, excluding the enclosing record itself.
//!
//! # See also
//! - [`super::frame`] for the published activation.
//! - [`super::metadata::CodeObjectMetadata`] for table ownership.

use super::{FrameStateId, NO_FRAME_STATE, SafepointId};

/// Rust resolver behind a machine-visible [`CodeRegistryView`].
pub type SafepointResolverFn = unsafe extern "C" fn(
    context: u64,
    code_object_id: u64,
    safepoint_id: SafepointId,
) -> *const SafepointRecord;

/// Resolver of one absolute return address in an expected caller generation.
pub type ReturnPcResolverFn = unsafe extern "C" fn(
    context: u64,
    code_object_id: u64,
    return_pc: u64,
) -> *const SafepointRecord;

/// Fixed code-registry lookup surface published on [`super::VmThread`].
#[repr(C, align(8))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CodeRegistryView {
    /// Opaque resolver-owned context address.
    pub context: u64,
    /// Address of a [`SafepointResolverFn`].
    pub resolve_safepoint: u64,
    /// Address of the isolate-owned array of stable function-cell addresses.
    /// Reload after any VM transition: linking may grow the directory.
    pub function_entries: u64,
    /// Number of indexed function identities in `function_entries`.
    pub function_entry_count: u64,
    /// Address of the exact code-id/return-PC resolver. No mapping history scan.
    pub resolve_return_pc: u64,
}

const _: [(); 16] = [(); std::mem::offset_of!(CodeRegistryView, function_entries)];

impl CodeRegistryView {
    /// Resolve one code-object-local safepoint record.
    ///
    /// # Safety
    /// The resolver and context must remain live for the active native call.
    #[must_use]
    pub unsafe fn resolve(
        self,
        code_object_id: u64,
        safepoint_id: SafepointId,
    ) -> Option<*const SafepointRecord> {
        if self.resolve_safepoint == 0 {
            return None;
        }
        // SAFETY: guaranteed by the registry publisher.
        let resolver: SafepointResolverFn = unsafe {
            std::mem::transmute::<usize, SafepointResolverFn>(self.resolve_safepoint as usize)
        };
        let record = unsafe { resolver(self.context, code_object_id, safepoint_id) };
        (!record.is_null()).then_some(record)
    }
    /// Resolve a genuine return address in exactly the expected code object.
    ///
    /// # Safety
    /// The resolver/context and owning code mapping remain live for this call.
    pub unsafe fn resolve_return(
        self,
        code_object_id: u64,
        return_pc: u64,
    ) -> Option<*const SafepointRecord> {
        if self.resolve_return_pc == 0 || return_pc == 0 {
            return None;
        }
        let resolver: ReturnPcResolverFn = unsafe {
            std::mem::transmute::<usize, ReturnPcResolverFn>(self.resolve_return_pc as usize)
        };
        let record = unsafe { resolver(self.context, code_object_id, return_pc) };
        (!record.is_null()).then_some(record)
    }
}

/// Live tagged native spill slots at one safepoint: a bitmap over the code
/// object's spill area, as V8's safepoint table records tagged stack slots.
///
/// A raw machine register is never a root at a collecting boundary (the
/// register map saves it to its spill home first), so a slot index is the
/// only location a record names.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct SpillRoots {
    /// Bit `i` set when spill slot `i` holds a live tagged value. The last
    /// word is nonzero unless the set is empty.
    words: Box<[u64]>,
}

impl SpillRoots {
    /// The set of the given spill-slot indices.
    #[must_use]
    pub fn from_slots(slots: impl IntoIterator<Item = u16>) -> Self {
        let mut words: Vec<u64> = Vec::new();
        for slot in slots {
            let word = usize::from(slot) / 64;
            if words.len() <= word {
                words.resize(word + 1, 0);
            }
            words[word] |= 1 << (slot % 64);
        }
        Self {
            words: words.into_boxed_slice(),
        }
    }

    /// Whether no slot is rooted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    /// Number of rooted slots.
    #[must_use]
    pub fn len(&self) -> usize {
        self.words
            .iter()
            .map(|word| word.count_ones() as usize)
            .sum()
    }

    /// One past the highest rooted slot index; zero when empty.
    #[must_use]
    pub fn end(&self) -> usize {
        self.words.last().map_or(0, |last| {
            (self.words.len() - 1) * 64 + (64 - last.leading_zeros() as usize)
        })
    }

    /// Rooted slot indices in ascending order.
    pub fn iter(&self) -> impl Iterator<Item = u16> + '_ {
        self.words.iter().enumerate().flat_map(|(index, &word)| {
            let mut bits = word;
            std::iter::from_fn(move || {
                (bits != 0).then(|| {
                    let bit = bits.trailing_zeros();
                    bits &= bits - 1;
                    (index * 64 + bit as usize) as u16
                })
            })
        })
    }

    /// Owned bitmap bytes.
    #[must_use]
    pub fn retained_bytes(&self) -> u64 {
        std::mem::size_of_val(self.words.as_ref()) as u64
    }
}

/// Exact machine return address association, owned by one code generation.
/// Source and traced locations live only in the referenced SafepointRecord.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SafepointEntry {
    /// Offset immediately after a real CALL/BLR in the complete mapping.
    pub native_return_offset: u32,
    /// Code-object-local source and root recipe.
    pub safepoint_id: SafepointId,
}
const _: [(); 8] = [(); std::mem::size_of::<SafepointEntry>()];

/// VM-owned expanded root map used by the current collector integration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SafepointRecord {
    /// Stable safepoint id.
    pub id: SafepointId,
    /// Frame-state snapshot or [`NO_FRAME_STATE`].
    pub frame_state: FrameStateId,
    /// Native spill slots holding live tagged values, beyond the canonical
    /// register window the collector always traces.
    pub spill_roots: SpillRoots,
    /// Logical inline descendants, with values indexed into the precise spill map.
    /// The owning code generation and this record's id select the recipe.
    pub inline_frames: Box<[crate::deopt::DeoptFrame<Option<u16>>]>,
    /// Canonical PC, in the code object's own function, of the generated
    /// JavaScript call this safepoint belongs to, or [`NO_CALL_PC`]. Stack
    /// walks read a calling frame's position here instead of a per-call PC
    /// store.
    pub call_pc: u32,
}

/// [`SafepointRecord::call_pc`] of a safepoint that is not a generated call.
pub const NO_CALL_PC: u32 = u32::MAX;

impl SafepointRecord {
    /// A record rooting nothing beyond the canonical register window, which
    /// the collector traces for every published frame.
    #[must_use]
    pub fn window(id: SafepointId, frame_state: FrameStateId) -> Self {
        Self {
            inline_frames: Box::default(),
            call_pc: NO_CALL_PC,
            id,
            frame_state,
            spill_roots: SpillRoots::default(),
        }
    }

    /// Owned heap backing retained by this root record, excluding the record
    /// itself: the spill bitmap and all inline frame slot slices, allocated
    /// for the generation's complete lifetime.
    #[must_use]
    pub fn retained_bytes(&self) -> u64 {
        self.inline_frames.iter().fold(
            self.spill_roots
                .retained_bytes()
                .saturating_add(std::mem::size_of_val(self.inline_frames.as_ref()) as u64),
            |bytes, frame| bytes.saturating_add(std::mem::size_of_val(frame.slots.as_ref()) as u64),
        )
    }

    /// Whether this map can reconstruct interpreter-visible state.
    #[must_use]
    pub fn has_deopt_state(&self) -> bool {
        self.frame_state != NO_FRAME_STATE
    }
}

const _: [(); 40] = [(); std::mem::size_of::<CodeRegistryView>()];
const _: [(); 8] = [(); std::mem::align_of::<CodeRegistryView>()];
const _: [(); 8] = [(); std::mem::size_of::<SafepointEntry>()];
const _: [(); 4] = [(); std::mem::align_of::<SafepointEntry>()];
const _: [(); 0] = [(); std::mem::offset_of!(SafepointEntry, native_return_offset)];
const _: [(); 4] = [(); std::mem::offset_of!(SafepointEntry, safepoint_id)];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_record_roots_no_slot_beyond_the_traced_window() {
        let record = SafepointRecord::window(3, NO_FRAME_STATE);
        assert!(record.spill_roots.is_empty());
        assert!(!record.has_deopt_state());
    }

    #[test]
    fn spill_roots_are_an_exact_ascending_bitmap() {
        let roots = SpillRoots::from_slots([130, 0, 63, 64, 0]);
        assert_eq!(roots.iter().collect::<Vec<_>>(), vec![0, 63, 64, 130]);
        assert_eq!(roots.len(), 4);
        assert_eq!(roots.end(), 131);
        assert_eq!(roots.retained_bytes(), 3 * 8);
        assert_eq!(SpillRoots::from_slots([]).end(), 0);
        assert!(SpillRoots::from_slots([]).is_empty());
    }

    #[test]
    fn retained_bytes_include_spill_bitmap_and_nested_inline_slots_once() {
        let record = SafepointRecord {
            id: 4,
            frame_state: NO_FRAME_STATE,
            spill_roots: SpillRoots::from_slots([0, 1, 70]),
            inline_frames: Box::new([
                crate::deopt::DeoptFrame::with_window(7, 11, None, [Some(0), None, Some(1)]),
                crate::deopt::DeoptFrame::with_window(8, 12, None, [Some(1)]),
            ]),
            call_pc: NO_CALL_PC,
        };
        let inline_slots: usize = record
            .inline_frames
            .iter()
            .map(|frame| std::mem::size_of_val(frame.slots.as_ref()))
            .sum();
        let expected =
            2 * 8 + 2 * std::mem::size_of::<crate::deopt::DeoptFrame<Option<u16>>>() + inline_slots;
        assert_eq!(record.retained_bytes(), expected as u64);
        let mut record = record;
        record.inline_frames = Box::default();
        assert_eq!(record.retained_bytes(), 2 * 8);
    }
}
