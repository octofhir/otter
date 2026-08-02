//! Which bodies still own storage the heap does not.
//!
//! A page image is the whole truth about an object only when the object
//! owns nothing outside the heap. [`crate::heap_image`] states that as an
//! invariant; this module is how it is checked instead of assumed.
//!
//! The test needs no per-type knowledge. Trace an object and ask the
//! tracer where the references it hands back actually live. A slot inside
//! the pointer cage is memory a page image carries and a restore
//! relocates. A slot outside it is a reference parked in memory the
//! collector does not own — the `Vec`, `Box`, or hash table a restore
//! cannot reproduce.
//!
//! This is the property V8's serializer enforces when it refuses to write
//! an object holding a raw external pointer, arrived at from the other
//! direction: rather than enumerate the fields that are allowed, ask the
//! tracer where the references are.
//!
//! # Telling a buffer from a temporary
//!
//! One idiom yields an out-of-cage slot and is nonetheless sound. A
//! compressed slot word carrying tag bits cannot be handed to a
//! `*mut RawGc` visitor directly, so its tracer forwards the bare offset
//! through a stack temporary and writes the re-tagged word back to the
//! live slot. Nothing is owned outside the heap; the address is simply a
//! stack local.
//!
//! Distinguishing that from a real buffer is exact rather than
//! heuristic: trace each object twice, the second time under an extra
//! stack frame. A stack temporary's address moves by the frame delta; a
//! buffer's does not. Only the addresses that hold still are counted as
//! escapes.
//!
//! # Contents
//!
//! - [`EscapeRow`] — one type tag's escaping bodies and slots.
//! - [`SelfContainmentAudit`] — the whole-heap result and its renderer.
//! - [`GcHeap::audit_self_containment`] — run one.
//!
//! # Invariants
//!
//! - A slot counts as contained when its address lies inside the pointer
//!   cage. Trailing storage and another body's cell both qualify; a
//!   `Vec` buffer does not.
//! - The walk visits every space, not just old: a body type that escapes
//!   is unrestorable wherever it currently sits, and tenuring moves it.
//! - Rows are sorted by descending escaping slots, ties by ascending tag,
//!   so the rendered table is stable across runs.
//! - The audit mutates nothing. Trace hooks that re-tag a word in place
//!   write back the value they read, because the visitor leaves slots
//!   alone.
//!
//! # See also
//!
//! - [`crate::heap_image`] — what the audit is a precondition for.
//! - [`crate::census`] — the flatter question of what is in each space.

use std::fmt::Write as _;

use crate::compressed::{RawGc, cage_base_addr, cage_size};
use crate::header::FREE_TAG;
use crate::heap::GcHeap;

/// One type tag's contribution to the audit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EscapeRow {
    /// `Traceable::TYPE_TAG` of the offending bodies.
    pub type_tag: u8,
    /// Rust type name registered under the tag, or `"?"`.
    pub type_name: &'static str,
    /// Live bodies of this type that were traced.
    pub objects: u64,
    /// Of those, how many held at least one reference outside the cage.
    pub escaping_objects: u64,
    /// References those bodies keep in memory the heap does not own.
    pub escaping_slots: u64,
}

/// Whole-heap self-containment result.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SelfContainmentAudit {
    /// Every tag that escaped, descending by `escaping_slots`.
    pub rows: Vec<EscapeRow>,
    /// Live bodies traced.
    pub objects_audited: u64,
    /// Pointer slots seen.
    pub slots_audited: u64,
    /// Pointer slots living in memory the heap does not own.
    pub escaping_slots: u64,
    /// Slots a tracer forwarded through a stack temporary and wrote
    /// back. Sound, and reported so the count is not mistaken for zero
    /// out-of-cage addresses.
    pub forwarded_slots: u64,
}

impl SelfContainmentAudit {
    /// `true` when every reference lives in memory a page image carries,
    /// so the heap could be captured and restored as page bytes.
    #[must_use]
    pub fn is_self_contained(&self) -> bool {
        self.escaping_slots == 0
    }

    /// Render the offending tags as a deterministic text table.
    #[must_use]
    pub fn render_text(&self) -> String {
        let mut out = String::with_capacity(1024);
        let _ = writeln!(
            out,
            "; self-containment — objects={} slots={} escaping={} forwarded={}",
            self.objects_audited, self.slots_audited, self.escaping_slots, self.forwarded_slots,
        );
        if self.rows.is_empty() {
            let _ = writeln!(out, "  every reference lives in memory the heap owns");
            return out;
        }
        let _ = writeln!(
            out,
            "  {:>4}  {:>9}  {:>9}  {:>9}  type",
            "tag", "objects", "escaping", "slots"
        );
        for row in &self.rows {
            let _ = writeln!(
                out,
                "  {:#04x}  {:>9}  {:>9}  {:>9}  {}",
                row.type_tag,
                row.objects,
                row.escaping_objects,
                row.escaping_slots,
                short_type_name(row.type_name),
            );
        }
        out
    }
}

fn short_type_name(name: &str) -> &str {
    match name.rsplit_once("::") {
        Some((_, leaf)) if !leaf.is_empty() => leaf,
        _ => name,
    }
}

/// Run `f` one stack frame deeper than the caller.
///
/// The padding is written and read through `black_box` so the frame
/// cannot be optimised away, which is what makes a stack temporary's
/// address differ between the two traces of the same object.
#[inline(never)]
fn under_a_deeper_stack<R>(f: impl FnOnce() -> R) -> R {
    let mut pad = [0u8; 4096];
    std::hint::black_box(&mut pad);
    let result = f();
    std::hint::black_box(&pad);
    result
}

/// Collect the out-of-cage slot addresses one trace of `header` yields,
/// in trace order.
///
/// # Safety
/// `header` must name a live body whose tag is `trace`'s registration.
unsafe fn out_of_cage_slots(
    header: *mut crate::header::GcHeader,
    trace: crate::trace::TraceFn,
    cage_lo: usize,
    cage_hi: usize,
    out: &mut Vec<usize>,
    slots_seen: &mut u64,
) {
    out.clear();
    // SAFETY: delegated to the caller's contract.
    unsafe {
        trace(header, &mut |slot: *mut RawGc| {
            *slots_seen += 1;
            let addr = slot as usize;
            if addr < cage_lo || addr >= cage_hi {
                out.push(addr);
            }
        });
    }
}

impl GcHeap {
    /// Check every live body for references held in memory the heap does
    /// not own.
    ///
    /// Runs under the same single-mutator contract as
    /// [`GcHeap::census`]: no allocator path may be open. Takes `&mut
    /// self` because a trace hook is free to rewrite a slot word in
    /// place; the visitor here hands every word back unchanged, so the
    /// heap is left exactly as it was.
    #[must_use]
    pub fn audit_self_containment(&mut self) -> SelfContainmentAudit {
        let mut objects = [0u64; 256];
        let mut escaping_objects = [0u64; 256];
        let mut escaping_slots = [0u64; 256];
        let mut audit = SelfContainmentAudit::default();

        let cage_lo = cage_base_addr();
        let cage_hi = cage_lo + cage_size();

        // The trace table is read-only for the duration; taking it out of
        // `self` by raw pointer lets the walk below hold `&mut` access to
        // page memory without borrowing the heap twice.
        let table: *const crate::trace::TraceTable = self.trace_table();

        // Reused across objects so the audit does not allocate per body.
        let mut shallow: Vec<usize> = Vec::new();
        let mut deep: Vec<usize> = Vec::new();

        for (_, pages) in self.census_spaces() {
            for page in pages {
                // SAFETY: every header up to `bump_cursor` was written by
                // the matching `bump_alloc`, and the single-mutator
                // contract means nothing is advancing it now.
                unsafe {
                    page.for_each_object(|header, _| {
                        let tag = (*header).type_tag();
                        if tag == FREE_TAG || (*header).is_swept() || (*header).is_forwarded() {
                            return;
                        }
                        let Some(trace) = (*table).get(tag) else {
                            return;
                        };
                        objects[tag as usize] += 1;
                        audit.objects_audited += 1;

                        out_of_cage_slots(
                            header,
                            trace,
                            cage_lo,
                            cage_hi,
                            &mut shallow,
                            &mut audit.slots_audited,
                        );
                        if shallow.is_empty() {
                            return;
                        }
                        // Some of those addresses may be stack temporaries a
                        // tracer forwards a tagged word through. Re-trace one
                        // frame deeper: a temporary moves, a buffer does not.
                        let mut ignored = 0u64;
                        under_a_deeper_stack(|| {
                            out_of_cage_slots(
                                header,
                                trace,
                                cage_lo,
                                cage_hi,
                                &mut deep,
                                &mut ignored,
                            );
                        });

                        let mut escaped_here = 0u64;
                        for (index, addr) in shallow.iter().enumerate() {
                            match deep.get(index) {
                                Some(again) if again == addr => escaped_here += 1,
                                // Moved with the stack, or the second trace
                                // yielded a different shape: not owned memory.
                                _ => audit.forwarded_slots += 1,
                            }
                        }
                        if escaped_here != 0 {
                            escaping_objects[tag as usize] += 1;
                            escaping_slots[tag as usize] += escaped_here;
                            audit.escaping_slots += escaped_here;
                        }
                    });
                }
            }
        }

        let mut rows: Vec<EscapeRow> = (0..256usize)
            .filter(|&tag| escaping_slots[tag] != 0)
            .map(|tag| EscapeRow {
                type_tag: tag as u8,
                type_name: self.trace_table().name(tag as u8).unwrap_or("?"),
                objects: objects[tag],
                escaping_objects: escaping_objects[tag],
                escaping_slots: escaping_slots[tag],
            })
            .collect();
        rows.sort_by(|a, b| {
            b.escaping_slots
                .cmp(&a.escaping_slots)
                .then_with(|| a.type_tag.cmp(&b.type_tag))
        });
        audit.rows = rows;
        audit
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compressed::{CAGE_TEST_LOCK, Gc};
    use crate::test_support::{OpaqueLeaf, OpaquePair, OpaqueVector};

    #[test]
    fn fixed_bodies_keep_their_references_inside_the_heap() {
        let _guard = CAGE_TEST_LOCK.lock().expect("cage test lock");
        let mut heap = GcHeap::new().expect("heap");
        let leaf = heap.alloc(OpaqueLeaf { payload: 1 }).expect("leaf");
        heap.alloc(OpaquePair {
            first: leaf.raw(),
            second: RawGc(0),
        })
        .expect("pair");
        let audit = heap.audit_self_containment();
        assert!(audit.is_self_contained(), "{}", audit.render_text());
        assert!(audit.slots_audited >= 2);
    }

    #[test]
    fn trailing_storage_counts_as_owned_memory() {
        let _guard = CAGE_TEST_LOCK.lock().expect("cage test lock");
        let mut heap = GcHeap::new().expect("heap");
        const LEN: usize = 32;
        let leaf = heap.alloc(OpaqueLeaf { payload: 5 }).expect("leaf");
        let vector: Gc<OpaqueVector> = heap
            .alloc_variable_with_roots(
                OpaqueVector::new(LEN),
                OpaqueVector::trailing_bytes(LEN),
                &mut |_| {},
            )
            .expect("vector");
        for index in 0..LEN {
            heap.with_payload(vector, |v| {
                v.set(index, leaf.raw());
                true
            });
        }
        let audit = heap.audit_self_containment();
        assert!(audit.is_self_contained(), "{}", audit.render_text());
    }

    #[test]
    fn a_body_holding_references_in_malloc_memory_is_reported() {
        let _guard = CAGE_TEST_LOCK.lock().expect("cage test lock");
        let mut heap = GcHeap::new().expect("heap");
        heap.register_traceable::<EscapingBody>();
        let leaf = heap.alloc(OpaqueLeaf { payload: 3 }).expect("leaf");
        heap.alloc(EscapingBody {
            outside: vec![leaf.raw(), leaf.raw()],
        })
        .expect("escaping body");
        let audit = heap.audit_self_containment();
        assert!(!audit.is_self_contained());
        let row = audit
            .rows
            .iter()
            .find(|r| r.type_tag == <EscapingBody as crate::trace::Traceable>::TYPE_TAG)
            .expect("row for the escaping body");
        assert_eq!(row.escaping_objects, 1);
        assert_eq!(row.escaping_slots, 2);
    }

    #[test]
    fn a_tracer_forwarding_through_a_temporary_is_not_an_escape() {
        let _guard = CAGE_TEST_LOCK.lock().expect("cage test lock");
        let mut heap = GcHeap::new().expect("heap");
        heap.register_traceable::<ForwardingBody>();
        let leaf = heap.alloc(OpaqueLeaf { payload: 4 }).expect("leaf");
        heap.alloc(ForwardingBody {
            // A tagged word: the low bits are payload, so the tracer
            // cannot hand the collector this field's address directly.
            tagged: leaf.raw().0 | 0b1,
        })
        .expect("forwarding body");
        let audit = heap.audit_self_containment();
        assert!(audit.is_self_contained(), "{}", audit.render_text());
        assert_eq!(audit.forwarded_slots, 1);
    }

    /// The shape the audit exists to catch: references parked in a `Vec`
    /// the collector does not own.
    struct EscapingBody {
        outside: Vec<RawGc>,
    }

    impl crate::trace::SafeTraceable for EscapingBody {
        const TYPE_TAG: u8 = 0xC7;

        fn trace_slots_safe(&mut self, visitor: &mut crate::trace::SlotVisitor<'_>) {
            for slot in &mut self.outside {
                visitor(slot as *mut RawGc);
            }
        }
    }

    /// The sound shape the audit must not mistake for the one above: a
    /// tagged word forwarded through a stack local and written back.
    struct ForwardingBody {
        tagged: u32,
    }

    impl crate::trace::SafeTraceable for ForwardingBody {
        const TYPE_TAG: u8 = 0xC8;

        fn trace_slots_safe(&mut self, visitor: &mut crate::trace::SlotVisitor<'_>) {
            let tag = self.tagged & 0b111;
            let mut bare = RawGc(self.tagged & !0b111);
            visitor(std::ptr::addr_of_mut!(bare));
            self.tagged = bare.0 | tag;
        }
    }
}
