//! Optimized code's subscriptions to monotone shape and context facts.
//!
//! An optimizing generation compiles under facts it does not re-read: a
//! shape no object has left stays that way, a defining field's value summary
//! keeps the word the compile captured, and a context slot the compile read
//! as a constant keeps its value. Installation verifies each fact and
//! subscribes the generation to it; the writer that clears the shape's
//! stability byte, widens its summary or writes the slot sets the
//! generation's [`DependencyFlag`]. Generated code tests its own flag where
//! it first relies on such a fact after entry or after code that can break
//! one, and leaves through a deopt when the flag is set.
//!
//! # Contents
//! - [`DependencyFlag`] — the set-once word one generation tests.
//! - [`ShapeFact`] — a subscribed kind of fact, also its shape's watch bit.
//! - [`ShapeSubscriptions`] — the registry-owned lists behind watched shapes.
//! - [`fire`] — sets the flags of a shape's subscribers to one kind, from any
//!   writer, without isolate context.
//! - [`ContextWatch`] / [`ContextSubscriptions`] — the watched slots of one
//!   context and their subscribers, named by the context's watch word.
//! - [`context_slot_written`] — a write to a watched slot: fire its
//!   subscribers and never watch the slot again, from any writer.
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
//! - A context's watch lives as long as the registry and moves with the
//!   context's watch word, so the word never dangles and a scavenge never
//!   detaches a context from its watch. Dependencies name the watch, not the
//!   context. A watched slot that is written is volatile from then on, in
//!   that context and in every other context of its scope, and is never
//!   watched again, so a binding that keeps changing — in one context or in
//!   each new one — cannot cycle compiles.
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

/// One context's watched slots and their subscriptions, owned by
/// [`ContextSubscriptions`] and named by the context's watch word.
///
/// Generated slot writers load [`Self::watched`] at offset zero and call the
/// writer boundary only when their slot's bit is set.
#[repr(C)]
pub struct ContextWatch {
    /// Bit `s`: some compile read slot `s` as a constant, so a write to it
    /// must fire.
    watched: std::sync::atomic::AtomicU64,
    /// Bit `s`: slot `s` was written after being watched.
    volatile: Cell<u64>,
    entries: Cell<Vec<(u16, Arc<DependencyFlag>)>>,
    /// This watch's identity in its registry, which dependencies name.
    id: u32,
    /// The volatile slots of every context of this context's scope, owned by
    /// the registry: a slot written after a watch here is never watched in
    /// any context of the scope again.
    scope: *const Cell<u64>,
}

const _: [(); 0] = [(); std::mem::offset_of!(ContextWatch, watched)];

impl Default for ContextWatch {
    fn default() -> Self {
        Self {
            watched: std::sync::atomic::AtomicU64::new(0),
            volatile: Cell::new(0),
            entries: Cell::new(Vec::new()),
            id: 0,
            scope: std::ptr::null(),
        }
    }
}

impl std::fmt::Debug for ContextWatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ContextWatch")
            .field("id", &self.id)
            .field("watched", &self.watched.load(Ordering::Relaxed))
            .field("volatile", &self.volatile.get())
            .finish_non_exhaustive()
    }
}

/// Slots a context watch can cover: one bit each.
pub const WATCHABLE_CONTEXT_SLOTS: u16 = 64;

impl ContextWatch {
    /// Whether slot `slot` is watched now.
    #[must_use]
    pub fn is_watched(&self, slot: u16) -> bool {
        slot < WATCHABLE_CONTEXT_SLOTS && self.watched.load(Ordering::Relaxed) >> slot & 1 != 0
    }
}

/// The isolate's context watches, by identity.
///
/// A context's watch word moves with the context, so a context watched while
/// young keeps its watch through every scavenge and promotion.
#[derive(Debug, Default)]
pub(crate) struct ContextSubscriptions {
    /// Watch `id` at index `id - 1`. Each watch is boxed: context watch
    /// words name its address, which must not move as the list grows.
    #[allow(clippy::vec_box)]
    watches: Vec<Box<ContextWatch>>,
    /// Volatile slots per scope descriptor, `(scope_function_id,
    /// scope_index)`: shared by every context of the scope.
    scopes: rustc_hash::FxHashMap<(u32, u16), Box<Cell<u64>>>,
}

impl ContextSubscriptions {
    /// Start watching slot `slot` of the live `context`, unless the slot is
    /// past the watchable range or was written after an earlier watch.
    /// Returns the identity of the context's watch when the slot is watched.
    pub(crate) fn watch_slot(
        &mut self,
        heap: &mut otter_gc::GcHeap,
        context: crate::context::ContextHandle,
        slot: u16,
    ) -> Option<u32> {
        if slot >= WATCHABLE_CONTEXT_SLOTS {
            return None;
        }
        let scope: &Cell<u64> = self
            .scopes
            .entry(crate::context::scope_identity(heap, context))
            .or_default();
        if scope.get() >> slot & 1 != 0 {
            return None;
        }
        let scope = std::ptr::from_ref(scope);
        let current = crate::context::watch(heap, context);
        let watch: &ContextWatch = if current == 0 {
            let id = u32::try_from(self.watches.len() + 1).ok()?;
            let fresh = Box::new(ContextWatch {
                id,
                scope,
                ..ContextWatch::default()
            });
            crate::context::set_watch(heap, context, std::ptr::from_ref(&*fresh) as u64);
            self.watches.push(fresh);
            self.watches.last().expect("just pushed")
        } else {
            // SAFETY: a nonzero watch word names a watch this registry owns.
            unsafe { &*(current as *const ContextWatch) }
        };
        if watch.volatile.get() >> slot & 1 != 0 {
            return None;
        }
        watch.watched.fetch_or(1 << slot, Ordering::Relaxed);
        Some(watch.id)
    }

    fn get(&self, id: u32) -> Option<&ContextWatch> {
        self.watches
            .get(usize::try_from(id).ok()?.checked_sub(1)?)
            .map(|watch| &**watch)
    }

    /// Subscribe `flag` to slot `slot` of the watch `id`. Returns `false`
    /// when the slot is no longer watched.
    pub(crate) fn subscribe(&mut self, id: u32, slot: u16, flag: &Arc<DependencyFlag>) -> bool {
        let Some(watch) = self.get(id) else {
            return false;
        };
        if !watch.is_watched(slot) {
            return false;
        }
        let mut entries = watch.entries.take();
        entries.push((slot, Arc::clone(flag)));
        watch.entries.set(entries);
        true
    }

    /// Whether slot `slot` of the watch `id` is still watched.
    pub(crate) fn holds(&self, id: u32, slot: u16) -> bool {
        self.get(id).is_some_and(|watch| watch.is_watched(slot))
    }

    /// Drop every subscription of `flag` to the watch `id`.
    pub(crate) fn unsubscribe(&mut self, id: u32, flag: &Arc<DependencyFlag>) {
        if let Some(watch) = self.get(id) {
            let mut entries = watch.entries.take();
            entries.retain(|(_, subscriber)| !Arc::ptr_eq(subscriber, flag));
            watch.entries.set(entries);
        }
    }
}

/// Slot `slot` of the context whose watch word is `watch` was just written:
/// when the slot is watched, set its subscribers' flags, drop them, and mark
/// the slot volatile.
pub(crate) fn context_slot_written(watch: u64, slot: u16) {
    // SAFETY: a nonzero watch word names the registry-owned watch of this
    // live context; only the mutator, which is running this writer, touches
    // it.
    let watch = unsafe { &*(watch as *const ContextWatch) };
    if !watch.is_watched(slot) {
        return;
    }
    watch.watched.fetch_and(!(1 << slot), Ordering::Relaxed);
    watch.volatile.set(watch.volatile.get() | 1 << slot);
    // SAFETY: the registry owns the scope's mask for as long as the watch.
    let scope = unsafe { &*watch.scope };
    scope.set(scope.get() | 1 << slot);
    let mut entries = watch.entries.take();
    entries.retain(|(subscribed, flag)| {
        if *subscribed == slot {
            flag.set();
            false
        } else {
            true
        }
    });
    watch.entries.set(entries);
}

/// Generated-code writer boundary: a store wrote slot `slot` of a context
/// whose watch word is `watch` and whose watch has that slot's bit set.
/// Cannot allocate, collect or reenter.
pub(crate) extern "C" fn context_slot_written_native(
    _heap: *mut otter_gc::GcHeap,
    watch: u64,
    slot: u64,
) -> crate::native_abi::NativeResultPair {
    match u16::try_from(slot) {
        Ok(slot) if watch != 0 => {
            context_slot_written(watch, slot);
            crate::native_abi::NativeResultPair::success(crate::Value::UNDEFINED)
        }
        _ => crate::native_abi::NativeResultPair::miss(),
    }
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

    #[test]
    fn an_unequal_int32_write_fires_constness_without_rearming_it() {
        let mut interpreter = Interpreter::new().expect("fixture interpreter");
        interpreter.with_handle_scope(|interpreter, scope| {
            let handle = interpreter.scoped_object_bare(scope).expect("object");
            let mut target = interpreter.escape_scoped(handle).as_object().unwrap();
            interpreter
                .create_data_property(&mut target, "value", Value::number_i32(7))
                .unwrap();
            let shape = object::shape(target, interpreter.gc_heap());
            assert_eq!(
                object::field_type::snapshot(shape),
                object::field_type::int32_sample(7)
            );
            let exact = DependencyFlag::new();
            let stable = DependencyFlag::new();
            let mut subscriptions = ShapeSubscriptions::default();
            subscriptions.watch(shape, ShapeFact::FieldType, &exact);
            subscriptions.watch(shape, ShapeFact::Stable, &stable);
            interpreter
                .create_data_property(&mut target, "value", Value::number_i32(7))
                .unwrap();
            assert!(!exact.is_set());
            interpreter
                .create_data_property(&mut target, "value", Value::number_i32(8))
                .unwrap();
            assert!(exact.is_set());
            assert!(!stable.is_set());
            assert_eq!(
                object::field_type::snapshot(shape),
                object::field_type::INT32
            );
            let category = DependencyFlag::new();
            subscriptions.watch(shape, ShapeFact::FieldType, &category);
            interpreter
                .create_data_property(&mut target, "value", Value::number_i32(7))
                .unwrap();
            assert!(!category.is_set());
            assert_eq!(
                object::field_type::snapshot(shape),
                object::field_type::INT32
            );
            interpreter
                .create_data_property(&mut target, "value", Value::number_f64(3.5))
                .unwrap();
            assert!(category.is_set());
            assert_eq!(object::field_type::snapshot(shape), object::field_type::TOP);
            for flag in [&exact, &category, &stable] {
                subscriptions.unwatch(shape, flag);
            }
            assert!(shape_body::watchers(shape).is_null());
        });
    }

    fn context(
        heap: &mut otter_gc::GcHeap,
        scope_index: u16,
        slot_count: u16,
    ) -> crate::context::ContextHandle {
        crate::context::alloc_context_with_roots(
            heap,
            crate::context::ContextShape {
                scope_function_id: 7,
                scope_index,
                slot_count,
                has_extension: false,
            },
            Value::undefined(),
            |_| false,
            &mut |_| {},
        )
        .expect("context")
    }

    #[test]
    fn a_written_slot_fires_its_subscribers_once_and_is_never_watched_again() {
        let mut heap = otter_gc::GcHeap::new().expect("heap");
        let target = context(&mut heap, 1, 3);
        let mut contexts = ContextSubscriptions::default();
        assert_eq!(crate::context::watch(&heap, target), 0);
        let id = contexts
            .watch_slot(&mut heap, target, 0)
            .expect("slot 0 watched");
        assert_eq!(contexts.watch_slot(&mut heap, target, 1), Some(id));
        assert_ne!(crate::context::watch(&heap, target), 0);
        let first = DependencyFlag::new();
        let second = DependencyFlag::new();
        assert!(contexts.subscribe(id, 0, &first));
        assert!(contexts.subscribe(id, 1, &second));

        // A slot no compile watches fires nothing.
        assert!(crate::context::write_slot(
            &mut heap,
            target,
            2,
            Value::number_i32(5)
        ));
        assert!(!first.is_set() && !second.is_set());
        assert!(crate::context::write_slot(
            &mut heap,
            target,
            0,
            Value::number_i32(1)
        ));
        assert!(first.is_set());
        assert!(!second.is_set());
        assert!(!contexts.holds(id, 0));
        assert!(contexts.holds(id, 1));

        // The written slot stays volatile: no later compile watches it.
        assert_eq!(contexts.watch_slot(&mut heap, target, 0), None);
        assert!(!contexts.subscribe(id, 0, &DependencyFlag::new()));

        // A retired subscriber is never fired, and the bits end at the
        // watchable range.
        contexts.unsubscribe(id, &second);
        assert!(crate::context::write_slot(
            &mut heap,
            target,
            1,
            Value::number_i32(2)
        ));
        assert!(!second.is_set());
        assert_eq!(
            contexts.watch_slot(&mut heap, target, WATCHABLE_CONTEXT_SLOTS),
            None
        );
    }

    #[test]
    fn a_written_slot_turns_volatile_in_every_context_of_its_scope() {
        let mut heap = otter_gc::GcHeap::new().expect("heap");
        let first = context(&mut heap, 1, 2);
        let second = context(&mut heap, 1, 2);
        let other_scope = context(&mut heap, 2, 2);
        let mut contexts = ContextSubscriptions::default();
        let id = contexts
            .watch_slot(&mut heap, first, 0)
            .expect("slot 0 watched");
        let flag = DependencyFlag::new();
        assert!(contexts.subscribe(id, 0, &flag));
        assert!(crate::context::write_slot(
            &mut heap,
            first,
            0,
            Value::number_i32(1)
        ));
        assert!(flag.is_set());

        // A binding that changed in one instance of its scope is read, not
        // folded, in every instance; its other slots and other scopes stay
        // watchable.
        assert_eq!(contexts.watch_slot(&mut heap, second, 0), None);
        let second_id = contexts
            .watch_slot(&mut heap, second, 1)
            .expect("slot 1 watched");
        assert_ne!(second_id, id);
        assert!(contexts.watch_slot(&mut heap, other_scope, 0).is_some());
    }

    #[test]
    fn a_watch_moves_with_its_young_context() {
        let mut heap = otter_gc::GcHeap::new().expect("heap");
        let target = context(&mut heap, 1, 1);
        let mut contexts = ContextSubscriptions::default();
        let id = contexts
            .watch_slot(&mut heap, target, 0)
            .expect("slot 0 watched");
        let word = crate::context::watch(&heap, target);
        let flag = DependencyFlag::new();
        assert!(contexts.subscribe(id, 0, &flag));
        let mut value = Value::context(target);
        let before = value.to_bits();
        for _ in 0..2 {
            let slot: *mut Value = &mut value;
            let mut roots = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
                // SAFETY: `value` lives across the collection.
                unsafe { (*slot).trace_value_slot_mut(visitor) };
            };
            heap.collect_minor_with_roots(&mut roots).expect("scavenge");
        }
        assert_ne!(value.to_bits(), before, "the young context moved");
        let moved = value.as_context().expect("still a context");
        assert_eq!(crate::context::watch(&heap, moved), word);
        assert_eq!(contexts.watch_slot(&mut heap, moved, 0), Some(id));
        assert!(crate::context::write_slot(
            &mut heap,
            moved,
            0,
            Value::number_i32(3)
        ));
        assert!(flag.is_set());
    }
}
