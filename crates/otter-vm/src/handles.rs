//! Scope-based GC handle rooting.
//!
//! Native Rust code building JS values holds raw [`Value`] cage offsets. The
//! young generation is a moving Cheney semispace, so any allocation can
//! relocate any young object and turn a copy held in a Rust local into a
//! dangling offset. The handle scope replaces the ad-hoc "thread `value_roots`
//! into every allocating call and re-read afterwards" contract with a single
//! rooted store — the [`HandleArena`] — that the collector rewrites in place.
//!
//! A [`Scoped`] handle carries only an index into that store, never a cached
//! payload, so every read resolves through the current slot and can never be
//! stale. Handles are minted inside [`Interpreter::with_handle_scope`], which
//! owns the arena range it opened and truncates back to it on return, so a
//! `Scoped` cannot outlive the scope that created it (the `'s` lifetime pins
//! it) and can never dangle.
//!
//! # Contents
//!
//! - [`HandleArena`] — contiguous, collector-traced handle storage; one per
//!   [`Interpreter`].
//! - [`HandleScope`] — a scope token owning an arena range `[base, len)`.
//! - [`Scoped`] — a `Copy` index handle whose lifetime pins it to its scope.
//!
//! # Invariants
//!
//! - **Every collection entry point that can run while native code is on the
//!   Rust stack must trace the arena.** There are exactly two: the extra-roots
//!   provider path walked during dispatch and the per-allocation snapshot path
//!   (`collect_runtime_roots`). Both reach
//!   [`crate::runtime_state::RuntimeState::trace_roots`], which walks the
//!   arena, so a slot is always current after any collection. A handle read
//!   through a live arena is therefore never stale.
//! - The arena is a strict stack: [`HandleScope`] truncation may only drop the
//!   range the scope opened, never slots an outer scope owns. Nesting is free —
//!   an inner scope's `base` is the current arena length, and its truncate
//!   leaves every outer slot in place.
//! - A `Scoped` never caches a payload; it is only an arena index. Reads go
//!   through [`HandleArena::get`], which the collector keeps live.
//!
//! # See also
//!
//! - [`crate::runtime_state`] — the root walker that traces the arena.
//! - [`crate::allocation_ops`] — the snapshot root path used by host-side
//!   allocations.

use std::marker::PhantomData;

use crate::{Interpreter, JsString, Value, VmError};

/// Contiguous scope-handle storage. One per [`crate::Interpreter`].
///
/// Every live slot is traced — and rewritten in place — by the runtime root
/// walk, so a parked [`Value`] always reflects the object's current location.
#[derive(Debug, Default)]
pub struct HandleArena {
    slots: Vec<Value>,
}

#[allow(dead_code)]
impl HandleArena {
    /// A fresh, empty arena.
    #[must_use]
    pub(crate) fn new() -> Self {
        Self { slots: Vec::new() }
    }

    /// Number of live handle slots. Used as the truncation base when a new
    /// scope opens.
    pub(crate) fn len(&self) -> usize {
        self.slots.len()
    }

    /// Park `v` and return its stable slot index for the lifetime of the
    /// owning scope.
    pub(crate) fn push(&mut self, v: Value) -> u32 {
        let idx = self.slots.len() as u32;
        self.slots.push(v);
        idx
    }

    /// Read the (possibly relocated) value parked at `idx`.
    pub(crate) fn get(&self, idx: u32) -> Value {
        self.slots[idx as usize]
    }

    /// Overwrite the value parked at `idx`. Used when an operation reallocates
    /// the handle it was given (e.g. an object shape transition).
    pub(crate) fn set(&mut self, idx: u32, v: Value) {
        self.slots[idx as usize] = v;
    }

    /// Drop every slot at or above `base`, restoring the arena to the state a
    /// scope found on entry.
    pub(crate) fn truncate(&mut self, base: usize) {
        self.slots.truncate(base);
    }

    /// Visit every live slot as a GC root. Called from the runtime root walk;
    /// the collector rewrites each slot to the relocated offset on a move.
    pub(crate) fn trace(&self, visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)) {
        for slot in &self.slots {
            slot.trace_value_slots(visitor);
        }
    }
}

/// Scope token. Created only by [`crate::Interpreter::with_handle_scope`]; owns
/// the arena range `[base, len)`, which is truncated when the scope exits.
///
/// Kept private-constructible so user code cannot forge one and hand a
/// [`Scoped`] a range it does not own.
#[allow(dead_code)]
pub struct HandleScope {
    base: usize,
}

#[allow(dead_code)]
impl HandleScope {
    /// Open a scope over the arena range starting at `base`.
    pub(crate) fn new(base: usize) -> Self {
        Self { base }
    }

    /// The arena length captured when this scope opened.
    pub(crate) fn base(&self) -> usize {
        self.base
    }
}

/// A rooted, always-current handle into the [`HandleArena`].
///
/// `Copy` and cheap: it carries only the arena index, never a payload. The `'s`
/// lifetime pins it inside the [`HandleScope`] that created it, so it cannot
/// escape the `with_handle_scope` closure and can never dangle.
#[derive(Clone, Copy)]
#[allow(dead_code)]
pub struct Scoped<'s> {
    idx: u32,
    _scope: PhantomData<&'s HandleScope>,
}

#[allow(dead_code)]
impl<'s> Scoped<'s> {
    /// Wrap an arena index as a handle pinned to scope `'s`.
    pub(crate) fn new(idx: u32) -> Self {
        Self {
            idx,
            _scope: PhantomData,
        }
    }

    /// The arena slot index this handle resolves through.
    pub(crate) fn index(self) -> u32 {
        self.idx
    }
}

/// Scope entry point and scoped allocation/access built on the arena.
///
/// These are the surface VM-internal callers use to build JS values without
/// hand-threading `value_roots`. They are consumed by the native-context
/// surface and interpreter-internal adoption; the `#[allow(dead_code)]` covers
/// the window before those callers land, keeping the core landable with its
/// own test coverage.
#[allow(dead_code)]
impl Interpreter {
    /// Open a handle scope, run `f`, then truncate the arena back to the length
    /// it had on entry.
    ///
    /// Handles minted inside `f` (via `scoped_*`) borrow the `&HandleScope`
    /// token, not the interpreter, so allocating calls interleave freely with
    /// live handles. The `'s` lifetime pins every [`Scoped`] to the closure, so
    /// none can escape. Truncation happens before the closure's result is
    /// returned; an early `?` inside `f` propagates through the returned `R`,
    /// so the truncation still runs on the normal return path here.
    pub(crate) fn with_handle_scope<R>(
        &mut self,
        f: impl FnOnce(&mut Interpreter, &HandleScope) -> R,
    ) -> R {
        let base = self.handle_arena.len();
        let scope = HandleScope::new(base);
        let r = f(self, &scope);
        self.handle_arena.truncate(base);
        r
    }

    /// Root an incoming raw `Value` in the current scope and hand back a
    /// [`Scoped`] handle to it.
    pub(crate) fn scoped_value<'s>(&mut self, _scope: &'s HandleScope, value: Value) -> Scoped<'s> {
        let idx = self.handle_arena.push(value);
        Scoped::new(idx)
    }

    /// Allocate a string and park it in the current scope.
    ///
    /// `JsString::from_str` can relocate young objects; every prior handle in
    /// the arena is traced across that allocation, and the freshly built string
    /// is parked immediately, so nothing goes stale.
    pub(crate) fn scoped_string<'s>(
        &mut self,
        scope: &'s HandleScope,
        text: &str,
    ) -> Result<Scoped<'s>, VmError> {
        let string = JsString::from_str(text, &mut self.gc_heap)?;
        Ok(self.scoped_value(scope, Value::string(string)))
    }

    /// Allocate a bare (null-prototype) object and park it in the current
    /// scope. The allocation snapshots the runtime roots (including the arena),
    /// so prior handles survive any collection it drives.
    pub(crate) fn scoped_object<'s>(
        &mut self,
        scope: &'s HandleScope,
    ) -> Result<Scoped<'s>, VmError> {
        let object = self.alloc_runtime_rooted_object_with_roots(&[], &[])?;
        Ok(self.scoped_value(scope, Value::object(object)))
    }

    /// Read property `key` from the object handle `obj`, resolving `obj`
    /// through the arena at call time, and park the result in the current
    /// scope. Absent properties read back as `undefined`.
    pub(crate) fn scoped_get<'s>(
        &mut self,
        scope: &'s HandleScope,
        obj: Scoped<'_>,
        key: &str,
    ) -> Result<Scoped<'s>, VmError> {
        let object = self
            .handle_arena
            .get(obj.index())
            .as_object()
            .ok_or(VmError::TypeMismatch)?;
        let value = crate::object::get(object, &self.gc_heap, key).unwrap_or_else(Value::undefined);
        Ok(self.scoped_value(scope, value))
    }

    /// Write `value` to property `key` on the object handle `obj`, resolving
    /// both handles through the arena at call time. A shape transition can
    /// reallocate the object, so the fresh handle is parked back into `obj`'s
    /// slot to keep it current.
    pub(crate) fn scoped_set(
        &mut self,
        _scope: &HandleScope,
        obj: Scoped<'_>,
        key: &str,
        value: Scoped<'_>,
    ) -> Result<(), VmError> {
        let mut object = self
            .handle_arena
            .get(obj.index())
            .as_object()
            .ok_or(VmError::TypeMismatch)?;
        let stored = self.handle_arena.get(value.index());
        crate::object::set(&mut object, &mut self.gc_heap, key, stored);
        self.handle_arena.set(obj.index(), Value::object(object));
        Ok(())
    }

    /// Read the current raw `Value` behind a scope handle for immediate
    /// hand-off across the scope boundary (returning to the VM, or storing into
    /// an already-rooted object). Valid until the next allocation.
    pub(crate) fn escape_scoped(&self, handle: Scoped<'_>) -> Value {
        self.handle_arena.get(handle.index())
    }
}

#[cfg(test)]
impl Interpreter {
    /// Current scope-handle arena length.
    fn handle_arena_len_for_test(&self) -> usize {
        self.handle_arena.len()
    }

    /// Force a minor collection while tracing the full runtime root set
    /// (including the handle arena), mirroring the host-side snapshot path so
    /// tests can drive a relocation with handles live.
    fn collect_minor_tracing_runtime_roots(&mut self) {
        let roots = self.collect_runtime_roots();
        self.gc_heap.collect_minor_with_roots(&mut |visitor| {
            for &slot in &roots {
                visitor(slot);
            }
        });
    }

    /// Force a full (mark-sweep) collection while tracing the full runtime root
    /// set. Old-space objects (e.g. string bodies) do not move, but anything
    /// the root walk cannot reach is swept — so surviving one proves the arena
    /// keeps a handle live.
    fn collect_full_tracing_runtime_roots(&mut self) {
        let roots = self.collect_runtime_roots();
        self.gc_heap.collect_full(&mut |visitor| {
            for &slot in &roots {
                visitor(slot);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn imm(i: i32) -> Value {
        Value::number_i32(i)
    }

    #[test]
    fn push_read_truncate() {
        let mut arena = HandleArena::new();
        assert_eq!(arena.len(), 0);

        let a = arena.push(imm(10));
        let b = arena.push(imm(20));
        let c = arena.push(imm(30));
        assert_eq!((a, b, c), (0, 1, 2));
        assert_eq!(arena.len(), 3);

        assert_eq!(arena.get(a).as_i32(), Some(10));
        assert_eq!(arena.get(c).as_i32(), Some(30));

        arena.set(b, imm(99));
        assert_eq!(arena.get(b).as_i32(), Some(99));

        arena.truncate(1);
        assert_eq!(arena.len(), 1);
        assert_eq!(arena.get(a).as_i32(), Some(10));
    }

    #[test]
    fn nested_truncate_leaves_outer_slots() {
        let mut arena = HandleArena::new();
        let outer = arena.push(imm(1));

        // An inner scope opens at the current length and only owns what it
        // pushes; truncating back to that base must not touch `outer`.
        let inner_base = arena.len();
        arena.push(imm(2));
        arena.push(imm(3));
        assert_eq!(arena.len(), 3);

        arena.truncate(inner_base);
        assert_eq!(arena.len(), 1);
        assert_eq!(arena.get(outer).as_i32(), Some(1));
    }

    #[test]
    fn scope_truncates_on_return() {
        let mut interp = Interpreter::new();
        let base = interp.handle_arena_len_for_test();
        let out = interp.with_handle_scope(|interp, s| {
            let v = interp.scoped_value(s, imm(7));
            interp.escape_scoped(v).as_i32()
        });
        assert_eq!(out, Some(7));
        // The scope range is gone once the wrapper returns.
        assert_eq!(interp.handle_arena_len_for_test(), base);
    }

    #[test]
    fn scoped_object_survives_and_moves_under_minor_gc() {
        let mut interp = Interpreter::new();
        let (moved, content) = interp.with_handle_scope(|interp, s| {
            // Objects are young-space, so a minor scavenge relocates a survivor
            // — the case that turns a raw held offset stale. Park a string
            // property so we can prove the whole object is intact post-move.
            let obj = interp.scoped_object(s).unwrap();
            let value = interp.scoped_string(s, "payload").unwrap();
            interp.scoped_set(s, obj, "k", value).unwrap();
            let before = interp
                .escape_scoped(obj)
                .as_raw_gc()
                .expect("object is a heap cell")
                .0;

            // Force minor collections until the survivor is evacuated to the
            // other semispace (its offset changes), proving the arena slot was
            // rewritten in place rather than left dangling. Cheney evacuation
            // moves a young survivor on the first flip; the bounded loop guards
            // against promotion having already parked it in old space.
            let mut after = before;
            let mut moved = false;
            for _ in 0..8 {
                // Churn young space so a collection has something to evacuate.
                let _ = interp.scoped_object(s).unwrap();
                interp.collect_minor_tracing_runtime_roots();
                after = interp
                    .escape_scoped(obj)
                    .as_raw_gc()
                    .expect("object still a heap cell after gc")
                    .0;
                if after != before {
                    moved = true;
                    break;
                }
            }
            assert!(
                moved,
                "scoped object never relocated across a minor GC (before={before}, after={after}); \
                 the move test did not exercise a relocation",
            );

            // Read the property back through the relocated handle: the arena
            // rewrote the object slot, and the object's own slots were fixed up
            // by the scavenge.
            let read_back = interp.scoped_get(s, obj, "k").unwrap();
            let content = interp
                .escape_scoped(read_back)
                .as_string(interp.gc_heap())
                .expect("property value still a string")
                .to_lossy_string(interp.gc_heap());
            (moved, content)
        });
        assert!(moved);
        assert_eq!(content, "payload");
    }

    #[test]
    fn scoped_string_survives_full_gc() {
        let mut interp = Interpreter::new();
        // String bodies live in old space (mark-sweep, non-moving), so the
        // relocation proof uses an object above. Here a full GC would sweep any
        // unreachable old-space body; surviving one proves the arena roots the
        // string. Only the arena keeps it live.
        let content = interp.with_handle_scope(|interp, s| {
            let a = interp.scoped_string(s, "hello handle scope").unwrap();
            interp.collect_full_tracing_runtime_roots();
            interp
                .escape_scoped(a)
                .as_string(interp.gc_heap())
                .expect("slot still holds a string after full gc")
                .to_lossy_string(interp.gc_heap())
        });
        assert_eq!(content, "hello handle scope");
    }

    #[test]
    fn inner_scope_truncation_leaves_outer_handle_valid() {
        let mut interp = Interpreter::new();
        let content = interp.with_handle_scope(|interp, outer| {
            let outer_str = interp.scoped_string(outer, "outer").unwrap();

            // An inner scope allocates and forces a collection, then exits and
            // truncates its own range. The outer handle must still resolve.
            interp.with_handle_scope(|interp, inner| {
                let _tmp = interp.scoped_string(inner, "inner-temp").unwrap();
                interp.collect_minor_tracing_runtime_roots();
            });

            interp
                .escape_scoped(outer_str)
                .as_string(interp.gc_heap())
                .expect("outer handle valid after inner scope closed")
                .to_lossy_string(interp.gc_heap())
        });
        assert_eq!(content, "outer");
    }

    #[test]
    fn stress_scoped_creation_with_interleaved_scavenges() {
        let mut interp = Interpreter::new();
        interp.with_handle_scope(|interp, s| {
            for i in 0..1000 {
                interp.with_handle_scope(|interp, inner| {
                    let text = format!("value-{i}");
                    let str_handle = interp.scoped_string(inner, &text).unwrap();
                    let obj = interp.scoped_object(inner).unwrap();
                    interp.scoped_set(inner, obj, "k", str_handle).unwrap();

                    // Force a scavenge with the handles live, then verify both
                    // the string content and the property read survived the
                    // relocation.
                    interp.collect_minor_tracing_runtime_roots();

                    let read_back = interp.scoped_get(inner, obj, "k").unwrap();
                    let content = interp
                        .escape_scoped(read_back)
                        .as_string(interp.gc_heap())
                        .expect("property value still a string")
                        .to_lossy_string(interp.gc_heap());
                    assert_eq!(content, text, "iteration {i} lost its string across gc");
                });
            }
            // The outer scope only holds the single opening frame after all
            // inner scopes truncated.
            assert_eq!(interp.handle_arena_len_for_test(), s.base());
        });
    }
}
