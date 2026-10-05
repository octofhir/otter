//! Bounded, nonallocating traversal of immutable UTF-16 string content.
//!
//! # Contents
//! - `CodeUnits` streams contiguous, sliced and rope bodies in source order.
//! - A fixed traversal frontier follows the sole rope-depth invariant.
//!
//! # Invariants
//! The immutable heap borrow excludes allocation, collection and mutation for
//! the cursor lifetime. No character pointer escapes a payload read. Each rope
//! edge is visited once; leaves are read sequentially. The fixed frontier is
//! sufficient for MAX_ROPE_DEPTH plus the one collapsed slice hop.
//!
//! # See also
//! - `super::gc_body` owns representation, depth and immutable code units.

use super::gc_body::{JsStringBodyRepr, JsStringHandle, MAX_ROPE_DEPTH};
use otter_gc::GcHeap;

#[derive(Clone, Copy)]
struct Segment {
    handle: JsStringHandle,
    start: u32,
    len: u32,
}
impl Segment {
    const EMPTY: Self = Self {
        handle: JsStringHandle::null(),
        start: 0,
        len: 0,
    };
}

/// This cursor owns no heap allocation or root: the borrowed heap cannot move.
pub(super) struct CodeUnits<'a> {
    heap: &'a GcHeap,
    pending: [Segment; MAX_ROPE_DEPTH as usize + 2],
    pending_len: usize,
    leaf: Segment,
    remaining: u32,
}

impl<'a> CodeUnits<'a> {
    pub(super) fn new(heap: &'a GcHeap, handle: JsStringHandle) -> Self {
        let len = heap.read_payload(handle, |body| body.len);
        let mut cursor = Self {
            heap,
            pending: [Segment::EMPTY; MAX_ROPE_DEPTH as usize + 2],
            pending_len: 0,
            leaf: Segment::EMPTY,
            remaining: len,
        };
        cursor.push(Segment {
            handle,
            start: 0,
            len,
        });
        cursor
    }

    fn push(&mut self, segment: Segment) {
        if segment.len == 0 {
            return;
        }
        assert!(
            self.pending_len < self.pending.len(),
            "verified string rope depth"
        );
        self.pending[self.pending_len] = segment;
        self.pending_len += 1;
    }
}

impl Iterator for CodeUnits<'_> {
    type Item = u16;
    fn next(&mut self) -> Option<u16> {
        loop {
            if self.leaf.len != 0 {
                let index = self.leaf.start as usize;
                let unit = self
                    .heap
                    .read_payload(self.leaf.handle, |body| match &body.repr {
                        JsStringBodyRepr::InlineFlat(units) => units[index],
                        JsStringBodyRepr::SeqFlat => body.seq_flat_units()[index],
                        JsStringBodyRepr::InlineLatin1(bytes) => u16::from(bytes[index]),
                        JsStringBodyRepr::SeqLatin1 => u16::from(body.seq_latin1_bytes()[index]),
                        _ => unreachable!("cursor leaf was classified contiguously"),
                    });
                self.leaf.start += 1;
                self.leaf.len -= 1;
                self.remaining -= 1;
                return Some(unit);
            }
            if self.pending_len == 0 {
                return None;
            }
            self.pending_len -= 1;
            let segment = self.pending[self.pending_len];
            enum Walk {
                Leaf,
                Slice(JsStringHandle, u32),
                Cons(JsStringHandle, JsStringHandle, u32),
            }
            let walk = self.heap.read_payload(segment.handle, |body| {
                debug_assert!(
                    segment
                        .start
                        .checked_add(segment.len)
                        .is_some_and(|end| end <= body.len)
                );
                match &body.repr {
                    JsStringBodyRepr::Cons { left, right, .. } => Walk::Cons(
                        *left,
                        *right,
                        self.heap.read_payload(*left, |body| body.len),
                    ),
                    JsStringBodyRepr::Sliced { parent, start } => Walk::Slice(*parent, *start),
                    _ => Walk::Leaf,
                }
            });
            match walk {
                Walk::Leaf => self.leaf = segment,
                Walk::Slice(parent, start) => self.push(Segment {
                    handle: parent,
                    start: start
                        .checked_add(segment.start)
                        .expect("collapsed slice range"),
                    len: segment.len,
                }),
                Walk::Cons(left, right, left_len) => {
                    let left_take = left_len.saturating_sub(segment.start).min(segment.len);
                    let right_take = segment.len - left_take;
                    self.push(Segment {
                        handle: right,
                        start: segment.start.saturating_sub(left_len),
                        len: right_take,
                    });
                    self.push(Segment {
                        handle: left,
                        start: segment.start,
                        len: left_take,
                    });
                }
            }
        }
    }
    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.remaining as usize;
        (remaining, Some(remaining))
    }
}
impl ExactSizeIterator for CodeUnits<'_> {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Interpreter, JsString, Value};

    #[test]
    fn streaming_preserves_rope_slice_width_and_surrogate_order_without_heap_effects() {
        let mut vm = Interpreter::new().expect("string cursor interpreter");
        vm.gc_heap.set_gc_stress(1, false);
        vm.with_handle_scope(|vm, scope| {
            let initial =
                JsString::from_utf16_units(&[0x100, 0xd800, 0x61], &mut vm.gc_heap).unwrap();
            let mut rope = vm.scoped_value(scope, Value::string(initial));
            let leaf =
                JsString::from_latin1(b"abcdefghijklmnopqrstuvwxy", &mut vm.gc_heap).unwrap();
            let leaf = vm.scoped_value(scope, Value::string(leaf));
            for _ in 0..300 {
                let a = vm.escape_scoped(rope).as_string(&vm.gc_heap).unwrap();
                let b = vm.escape_scoped(leaf).as_string(&vm.gc_heap).unwrap();
                let joined = JsString::concat(a, b, &mut vm.gc_heap).unwrap();
                rope = vm.scoped_value(scope, Value::string(joined));
            }
            let current = vm.escape_scoped(rope).as_string(&vm.gc_heap).unwrap();
            let expected = current.to_utf16_vec(&vm.gc_heap);
            let before = vm.gc_heap.gc_stats().clone();
            let actual: Vec<_> = CodeUnits::new(&vm.gc_heap, current.handle()).collect();
            assert_eq!(actual, expected);
            assert_eq!(actual[..3], [0x100, 0xd800, 0x61]);
            let after = vm.gc_heap.gc_stats().clone();
            assert_eq!(after.alloc_bytes_total, before.alloc_bytes_total);
            assert_eq!(after.minor_gc_cycles, before.minor_gc_cycles);
            let sliced = current.slice(1, 41, &mut vm.gc_heap).unwrap();
            let sliced = vm.scoped_value(scope, Value::string(sliced));
            let current = vm.escape_scoped(sliced).as_string(&vm.gc_heap).unwrap();
            assert_eq!(
                CodeUnits::new(&vm.gc_heap, current.handle()).collect::<Vec<_>>(),
                expected[1..42]
            );
            assert_eq!(CodeUnits::new(&vm.gc_heap, current.handle()).len(), 41);
        });
    }
}
