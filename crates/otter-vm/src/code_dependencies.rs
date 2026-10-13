//! Optimized code's subscriptions to monotone shape facts.
//!
//! An optimizing generation compiles under shape facts it does not re-read:
//! a shape no object has left stays that way, and a defining field's value
//! summary keeps the word the compile captured. Installation verifies each
//! fact and subscribes the generation to its shape; the writer that clears
//! the shape's stability byte or widens its summary sets the generation's
//! [`DependencyFlag`]. Generated code tests its own flag where it first
//! relies on such a fact after entry or after code that can break one, and
//! leaves through a deopt when the flag is set.
//!
//! # Contents
//! - [`DependencyFlag`] — the set-once word one generation tests.
//! - [`ShapeFact`] — a subscribed kind of fact, also its shape's watch bit.
//! - [`ShapeSubscriptions`] — the registry-owned lists behind watched shapes.
//! - [`fire`] — sets the flags of a shape's subscribers to one kind, from any
//!   writer, without isolate context.
//!
//! # Invariants
//! - Only the mutator subscribes, unsubscribes and fires, and generated code
//!   reads its flag on the mutator: a plain load sees every preceding fire.
//! - A shape's watch bits cover the kinds of its listed subscriptions, so a
//!   writer whose bits are clear has nothing to visit.
//! - A subscribed generation roots its shapes until it retires and
//!   unsubscribes before that root goes, so a shape with a list is live.
//! - A flag never clears. Firing drops the fired subscriptions from the list;
//!   the emptied list itself goes when its last subscriber retires.
//! - Every set flag of a registry's generations is counted until the
//!   interpreter drains the count and invalidates them, so a fired generation
//!   is not selected for entry while the tier policy still holds it.
//!
//! # See also
//! - [`crate::object::shape_body`] owns the watch byte and the list pointer.
//! - [`crate::jit_registry`] verifies and subscribes at installation.

use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU32, Ordering};

use crate::object::{ShapeHandle, shape_body};

/// The word one optimizing generation tests before relying on a subscribed
/// fact: zero while every fact holds, one forever after any breaks. Setting
/// it also counts it in its registry's fired count, which the interpreter
/// drains to retire the generation.
#[derive(Debug, Default)]
pub struct DependencyFlag {
    set: AtomicU8,
    fired: Arc<AtomicU32>,
}

impl DependencyFlag {
    /// A clear flag counted by no registry.
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// A clear flag counted in `fired`.
    pub(crate) fn counted(fired: &Arc<AtomicU32>) -> Arc<Self> {
        Arc::new(Self {
            set: AtomicU8::new(0),
            fired: Arc::clone(fired),
        })
    }

    /// Address generated code loads the flag byte from.
    #[must_use]
    pub fn address(&self) -> usize {
        self.set.as_ptr() as usize
    }

    /// Whether some subscribed fact has broken.
    #[must_use]
    pub fn is_set(&self) -> bool {
        self.set.load(Ordering::Relaxed) != 0
    }

    pub(crate) fn set(&self) {
        if self.set.swap(1, Ordering::Relaxed) == 0 {
            self.fired.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// A monotone shape fact a generation can subscribe to; the value is the
/// shape's watch bit for it.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShapeFact {
    /// No object has left the shape.
    Stable = 1,
    /// The shape's defining field summary keeps its captured word.
    FieldType = 2,
}

impl ShapeFact {
    /// Bit index of this fact in a shape's watch byte.
    #[must_use]
    pub const fn bit(self) -> u32 {
        (self as u8).trailing_zeros()
    }
}

/// One shape's subscriptions, owned by [`ShapeSubscriptions`] and named by
/// the shape's list pointer.
#[derive(Default)]
pub(crate) struct ShapeWatchers {
    entries: Cell<Vec<(ShapeFact, Arc<DependencyFlag>)>>,
}

impl std::fmt::Debug for ShapeWatchers {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let entries = self.entries.take();
        let result = formatter
            .debug_struct("ShapeWatchers")
            .field("entries", &entries)
            .finish();
        self.entries.set(entries);
        result
    }
}

impl ShapeWatchers {
    fn bits(entries: &[(ShapeFact, Arc<DependencyFlag>)]) -> u8 {
        entries.iter().fold(0, |bits, (fact, _)| bits | *fact as u8)
    }
}

/// The isolate's subscription lists by shape handle.
#[derive(Debug, Default)]
pub(crate) struct ShapeSubscriptions {
    lists: rustc_hash::FxHashMap<u32, Box<ShapeWatchers>>,
}

impl ShapeSubscriptions {
    /// Subscribe `flag` to `fact` of the live, rooted `shape`.
    pub(crate) fn watch(
        &mut self,
        shape: ShapeHandle,
        fact: ShapeFact,
        flag: &Arc<DependencyFlag>,
    ) {
        let list: &ShapeWatchers = self.lists.entry(shape.offset()).or_default();
        let mut entries = list.entries.take();
        entries.push((fact, Arc::clone(flag)));
        let bits = ShapeWatchers::bits(&entries);
        list.entries.set(entries);
        shape_body::set_watchers(shape, bits, std::ptr::from_ref(list));
    }

    /// Drop every subscription of `flag` to the live, rooted `shape`, and
    /// the shape's list with its last entry.
    pub(crate) fn unwatch(&mut self, shape: ShapeHandle, flag: &Arc<DependencyFlag>) {
        let Some(list) = self.lists.get(&shape.offset()).map(|list| &**list) else {
            return;
        };
        let mut entries = list.entries.take();
        entries.retain(|(_, subscriber)| !Arc::ptr_eq(subscriber, flag));
        let bits = ShapeWatchers::bits(&entries);
        if entries.is_empty() {
            self.lists.remove(&shape.offset());
            shape_body::set_watchers(shape, 0, std::ptr::null());
        } else {
            list.entries.set(entries);
            shape_body::set_watchers(shape, bits, std::ptr::from_ref(list));
        }
    }
}

/// `fact` of `shape` just changed: set every subscriber's flag and drop
/// those subscriptions. Writers call this only when the shape's watch bit
/// for `fact` is set.
pub(crate) fn fire(shape: ShapeHandle, fact: ShapeFact) {
    let list = shape_body::watchers(shape);
    if list.is_null() {
        return;
    }
    // SAFETY: a non-null list pointer names the registry-owned list of this
    // live shape; only the mutator, which is running this writer, touches it.
    let list = unsafe { &*list };
    let mut entries = list.entries.take();
    entries.retain(|(subscribed, flag)| {
        if *subscribed == fact {
            flag.set();
            false
        } else {
            true
        }
    });
    let bits = ShapeWatchers::bits(&entries);
    list.entries.set(entries);
    shape_body::set_watch_bits(shape, bits);
}

/// Generated-code writer boundary: an object left the shape whose cell
/// header is at `shape_address`, and that shape is watched. Cannot
/// allocate, collect or reenter.
pub(crate) extern "C" fn note_left_native(
    _heap: *mut otter_gc::GcHeap,
    shape_address: u64,
    _unused: u64,
) -> crate::native_abi::NativeResultPair {
    let base = otter_gc::cage_base() as u64;
    let Some(offset) = shape_address
        .checked_sub(base)
        .and_then(|offset| u32::try_from(offset).ok())
        .filter(|offset| *offset != 0 && offset.is_multiple_of(8))
    else {
        return crate::native_abi::NativeResultPair::miss();
    };
    // SAFETY: the caller's guards proved this a live shape the receiver held.
    let shape = unsafe { otter_gc::Gc::from_offset(offset) };
    shape_body::note_left(shape);
    crate::native_abi::NativeResultPair::success(crate::Value::UNDEFINED)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Interpreter, Value, object};

    #[test]
    fn leaving_and_widening_fire_only_their_own_subscribers() {
        let mut interpreter = Interpreter::new().expect("fixture interpreter");
        interpreter.with_handle_scope(|interpreter, scope| {
            let handle = interpreter.scoped_object_bare(scope).expect("object");
            let mut target = interpreter.escape_scoped(handle).as_object().unwrap();
            interpreter
                .create_data_property(&mut target, "x", Value::NULL)
                .unwrap();
            let shape = object::shape(target, interpreter.gc_heap());
            assert!(shape_body::is_stable(shape));
            let stable = DependencyFlag::new();
            let field = DependencyFlag::new();
            let mut subscriptions = ShapeSubscriptions::default();
            subscriptions.watch(shape, ShapeFact::Stable, &stable);
            subscriptions.watch(shape, ShapeFact::FieldType, &field);

            // An unchanged summary fires nothing; a widening fires the field
            // subscriber alone.
            interpreter
                .create_data_property(&mut target, "x", Value::NULL)
                .unwrap();
            assert!(!field.is_set());
            interpreter
                .create_data_property(&mut target, "x", Value::number_i32(1))
                .unwrap();
            assert!(field.is_set());
            assert!(!stable.is_set());

            // Leaving the shape fires its stability subscriber.
            interpreter
                .create_data_property(&mut target, "y", Value::NULL)
                .unwrap();
            assert!(stable.is_set());
            assert!(!shape_body::watchers(shape).is_null());

            // The fired subscriptions are gone; the list goes with its last
            // subscriber's retirement.
            subscriptions.unwatch(shape, &stable);
            subscriptions.unwatch(shape, &field);
            assert!(shape_body::watchers(shape).is_null());
        });
    }

    #[test]
    fn unwatching_keeps_other_subscribers_armed() {
        let mut interpreter = Interpreter::new().expect("fixture interpreter");
        interpreter.with_handle_scope(|interpreter, scope| {
            let handle = interpreter.scoped_object_bare(scope).expect("object");
            let mut target = interpreter.escape_scoped(handle).as_object().unwrap();
            interpreter
                .create_data_property(&mut target, "x", Value::NULL)
                .unwrap();
            let shape = object::shape(target, interpreter.gc_heap());
            let retired = DependencyFlag::new();
            let live = DependencyFlag::new();
            let mut subscriptions = ShapeSubscriptions::default();
            subscriptions.watch(shape, ShapeFact::Stable, &retired);
            subscriptions.watch(shape, ShapeFact::Stable, &live);
            subscriptions.unwatch(shape, &retired);
            interpreter
                .create_data_property(&mut target, "y", Value::NULL)
                .unwrap();
            assert!(live.is_set());
            assert!(!retired.is_set());
            subscriptions.unwatch(shape, &live);
            assert!(shape_body::watchers(shape).is_null());
        });
    }
}
