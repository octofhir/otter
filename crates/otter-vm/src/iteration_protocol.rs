//! The unmodified iteration protocol — V8's iterator protectors.
//!
//! GetIterator reads `@@iterator` from the iterable and `next` from the
//! iterator it returns; IteratorClose reads `return`. While a realm's
//! collection and iterator prototypes are as bootstrap left them, those
//! reads have known answers: iterating a plain Array, Map, Set or string
//! starts the built-in iterator its `@@iterator` would return, stepping a
//! built-in iterator is its built-in `next`, and closing it runs its
//! built-in `return` (or nothing).
//!
//! # Contents
//! - [`IterationProofs`] — per-realm proofs, one per iterable kind and one
//!   per built-in iterator prototype.
//! - [`Interpreter::unobservable_iterator_record`] — GetIterator with every
//!   read proven, or taken as built-in by runtime-internal code.
//! - [`Interpreter::builtin_next_proven`] / [`Interpreter::builtin_return_proven`]
//!   — a built-in iterator's `next` / `return` is the one its state implements.
//! - [`Interpreter::is_builtin_next`] — an observed `next` is that built-in.
//!
//! # Invariants
//! - A proof is the prototype-chain validity cell of the prototype it
//!   covers, taken after reading its answers off the chain without running
//!   code. Any write to a prototype on the chain retires the cell, and the
//!   next use reads the chain again.
//! - A receiver qualifies only when nothing of its own can shadow the chain
//!   (no symbol property on an array, no expando on a Map, Set or built-in
//!   iterator) and its `[[Prototype]]` is its realm's intrinsic.
//! - A proven path builds exactly the record the spec algorithm would.
//!
//! # See also
//! - [`crate::regexp_fast`] — the same scheme for RegExp.
//! - [`crate::iterator_ops`] — the observable protocol.

use std::sync::Arc;

use crate::iterator_state::{BuiltinIteratorOrigin, IteratorState};
use crate::native_function::NativeFastFn;
use crate::object::prototype_validity::{PrototypeValidity, chain_validity};
use crate::object::{self, JsObject, PropertyLookup, ShapeState};
use crate::symbol::{JsSymbol, WellKnown};
use crate::{Interpreter, MapIteratorKind, SetIteratorKind, Value};

/// Built-in iterables whose `@@iterator` a realm proves.
#[derive(Debug, Clone, Copy)]
enum Iterable {
    Array,
    Map,
    Set,
    String,
}

impl Iterable {
    fn origin(self) -> BuiltinIteratorOrigin {
        match self {
            Self::Array => BuiltinIteratorOrigin::Array,
            Self::Map => BuiltinIteratorOrigin::Map,
            Self::Set => BuiltinIteratorOrigin::Set,
            Self::String => BuiltinIteratorOrigin::String,
        }
    }
}

/// What a built-in iterator prototype's chain answers.
#[derive(Debug, Clone, Copy, Default)]
struct IteratorFacts {
    /// `next` is the built-in step of the iterator's state.
    next: bool,
    /// `return` is absent, or the built-in close of a helper or wrapper.
    close: bool,
    /// `@@iterator` is `%Iterator.prototype%[@@iterator]`, which returns
    /// its receiver.
    iterable: bool,
}

#[derive(Debug, Clone)]
struct Proof<T> {
    validity: Arc<PrototypeValidity>,
    facts: T,
}

impl<T: Copy> Proof<T> {
    fn current(proof: &Option<Self>) -> Option<T> {
        proof
            .as_ref()
            .filter(|proof| proof.validity.is_valid())
            .map(|proof| proof.facts)
    }
}

/// Per-realm iteration proofs, each valid while its cell is.
#[derive(Debug, Clone, Default)]
pub(crate) struct IterationProofs {
    iterables: [Option<Proof<bool>>; 4],
    iterators: [Option<Proof<IteratorFacts>>; 7],
}

fn origin_index(origin: BuiltinIteratorOrigin) -> usize {
    match origin {
        BuiltinIteratorOrigin::Array => 0,
        BuiltinIteratorOrigin::Map => 1,
        BuiltinIteratorOrigin::Set => 2,
        BuiltinIteratorOrigin::String => 3,
        BuiltinIteratorOrigin::RegExpString => 4,
        BuiltinIteratorOrigin::Helper => 5,
        BuiltinIteratorOrigin::WrapForValidIterator => 6,
    }
}

/// The built-in `next` of each iterator prototype.
fn builtin_next(origin: BuiltinIteratorOrigin) -> NativeFastFn {
    use crate::intrinsics::iterator as it;
    match origin {
        BuiltinIteratorOrigin::Array
        | BuiltinIteratorOrigin::Map
        | BuiltinIteratorOrigin::Set
        | BuiltinIteratorOrigin::String => it::iterator_proto_next,
        BuiltinIteratorOrigin::RegExpString => it::regexp_string_iterator_proto_next,
        BuiltinIteratorOrigin::Helper => it::iterator_helper_proto_next,
        BuiltinIteratorOrigin::WrapForValidIterator => it::wrap_for_valid_iterator_next,
    }
}

/// The built-in `return` of each iterator prototype; `None` when the chain
/// has none.
fn builtin_return(origin: BuiltinIteratorOrigin) -> Option<NativeFastFn> {
    use crate::intrinsics::iterator as it;
    match origin {
        BuiltinIteratorOrigin::Helper => Some(it::iterator_helper_proto_return),
        BuiltinIteratorOrigin::WrapForValidIterator => Some(it::iterator_proto_return),
        _ => None,
    }
}

#[derive(Clone, Copy)]
enum Key {
    Name(&'static str),
    Symbol(JsSymbol),
}

/// `[[Get]]` along an ordinary chain without running code: `Some(None)` when
/// absent, `Some(Some(value))` for a data property, `None` when an accessor
/// or a non-ordinary link leaves the answer to the observable protocol.
fn chain_data(heap: &otter_gc::GcHeap, first: JsObject, key: Key) -> Option<Option<Value>> {
    let mut current = first;
    for _ in 0..object::PROTO_CHAIN_HARD_CAP {
        let lookup = match key {
            Key::Name(name) => object::lookup_own(current, heap, name),
            Key::Symbol(symbol) => object::lookup_own_symbol(current, heap, symbol),
        };
        match lookup {
            PropertyLookup::Data { value, .. } => return Some(Some(value)),
            PropertyLookup::Accessor { .. } => return None,
            PropertyLookup::Absent => {}
        }
        match object::prototype_value(current, heap) {
            None => return Some(None),
            Some(next) => current = next.as_object()?,
        }
    }
    None
}

fn is_static(heap: &otter_gc::GcHeap, answer: Option<Option<Value>>, expected: NativeFastFn) -> bool {
    matches!(answer, Some(Some(value))
        if value.as_native_function().is_some_and(|f| f.is_static_fn(heap, expected)))
}

impl Interpreter {
    /// GetIterator(`value`) when no step of it is observable: the fresh
    /// built-in record of a proven iterable, a proven built-in iterator
    /// itself, or the record a generator drives. `primordial` code takes
    /// the built-in answers without the proofs. `None` leaves the observable
    /// protocol to the caller.
    pub(crate) fn unobservable_iterator_record(
        &mut self,
        stack: &crate::ActivationStack,
        value: Value,
        primordial: bool,
    ) -> Result<Option<Value>, crate::VmError> {
        self.with_handle_scope(|interp, scope| {
            let root = interp.scoped_value(scope, value);
            if let Some(kind) = interp.plain_iterable(value)
                && (primordial || interp.iterable_proven(kind))
            {
                let value = interp.escape_scoped(root);
                let state = proven_iterator_state(value, &interp.gc_heap)
                    .ok_or(crate::VmError::InvalidOperand)?;
                let iterator =
                    interp.alloc_stack_rooted_iterator_state(stack, state, &[&value], &[])?;
                return Ok(Some(Value::iterator(iterator)));
            }
            let value = interp.escape_scoped(root);
            if primordial && interp.plain_iterator_origin(value).is_some()
                || interp.proven_self_iterator(value)
            {
                return Ok(Some(interp.escape_scoped(root)));
            }
            let value = interp.escape_scoped(root);
            let Some(handle) = value.as_generator() else {
                return Ok(None);
            };
            let iterator = interp.alloc_stack_rooted_iterator_state(
                stack,
                IteratorState::Generator { handle },
                &[&value],
                &[],
            )?;
            Ok(Some(Value::iterator(iterator)))
        })
    }

    /// Whether the activation at `frame_index` runs runtime-internal code
    /// that iterates with intrinsic algorithms.
    pub(crate) fn frame_iterates_primordially(
        &self,
        context: &crate::ExecutionContext,
        stack: &crate::ActivationStack,
        frame_index: usize,
    ) -> bool {
        let function_id = stack[frame_index].function_id;
        context.for_function(function_id).is_ok_and(|owner| {
            owner
                .exec_function(function_id)
                .is_some_and(|function| function.primordial_iteration)
        })
    }

    /// Whether the nearest bytecode activation runs runtime-internal code:
    /// a built-in it called iterates on its behalf.
    pub(crate) fn caller_iterates_primordially(
        &self,
        context: &crate::ExecutionContext,
        stack: &crate::ActivationStack,
    ) -> bool {
        (0..stack.len())
            .rev()
            .find(|&index| stack[index].header.kind != crate::native_abi::NativeFrameKind::Host)
            .is_some_and(|index| self.frame_iterates_primordially(context, stack, index))
    }

    /// Whether a built-in iterating `value` for its caller may read it
    /// directly: its protocol is proven, or runtime-internal code called the
    /// built-in. May allocate; the caller roots `value`.
    pub(crate) fn intrinsic_iterable(
        &mut self,
        context: &crate::ExecutionContext,
        stack: &crate::ActivationStack,
        value: Value,
    ) -> bool {
        self.plain_iterable(value).is_some_and(|kind| {
            self.iterable_proven(kind) || self.caller_iterates_primordially(context, stack)
        })
    }

    /// The built-in iterable `value` is, when nothing of its own can shadow
    /// its realm prototype's `@@iterator`.
    fn plain_iterable(&self, value: Value) -> Option<Iterable> {
        let heap = &self.gc_heap;
        let (kind, own, link, intrinsic) = if let Some(array) = value.as_array() {
            let symbol = self.well_known_symbols.get(WellKnown::Iterator);
            (
                Iterable::Array,
                crate::array::get_symbol_descriptor(array, heap, symbol).is_some(),
                crate::array::prototype_override(array, heap),
                self.realm_intrinsics.array_prototype(),
            )
        } else if let Some(map) = value.as_map() {
            (
                Iterable::Map,
                crate::collections::map_expando(map, heap).is_some(),
                crate::collections::map_prototype_override(map, heap),
                self.realm_intrinsics.map_prototype(),
            )
        } else if let Some(set) = value.as_set() {
            (
                Iterable::Set,
                crate::collections::set_expando(set, heap).is_some(),
                crate::collections::set_prototype_override(set, heap),
                self.realm_intrinsics.set_prototype(),
            )
        } else if value.as_string(heap).is_some() {
            return Some(Iterable::String);
        } else {
            return None;
        };
        (!own && self.has_realm_prototype(link, intrinsic)).then_some(kind)
    }

    /// Whether a built-in iterator is proven to be its own GetIterator
    /// record, stepped by its built-in `next`.
    pub(crate) fn proven_self_iterator(&mut self, value: Value) -> bool {
        self.plain_iterator_origin(value).is_some_and(|origin| {
            let facts = self.iterator_facts(origin);
            facts.iterable && facts.next
        })
    }

    /// Whether GetIterator over a plain `kind` iterable is proven to start
    /// the built-in iterator and step it with the built-in `next`. May
    /// allocate the chain's prototype roles once.
    fn iterable_proven(&mut self, kind: Iterable) -> bool {
        self.iterable_is_builtin(kind) && self.iterator_facts(kind.origin()).next
    }

    /// Whether `Get(iterator, "next")` is the built-in step of its state.
    pub(crate) fn builtin_next_proven(&mut self, iterator: Value) -> bool {
        self.plain_iterator_origin(iterator)
            .is_some_and(|origin| self.iterator_facts(origin).next)
    }

    /// Whether `GetMethod(iterator, "return")` is the built-in close of its
    /// state (nothing, for the collection and string iterators).
    pub(crate) fn builtin_return_proven(&mut self, iterator: Value) -> bool {
        self.plain_iterator_origin(iterator)
            .is_some_and(|origin| self.iterator_facts(origin).close)
    }

    /// Whether an observed `next` is the built-in step of `iterator`'s state.
    pub(crate) fn is_builtin_next(&self, iterator: Value, next: Value) -> bool {
        let Some(handle) = iterator.as_iterator() else {
            return false;
        };
        let heap = &self.gc_heap;
        heap.read_payload(handle, IteratorState::builtin_origin)
            .is_some_and(|origin| is_static(heap, Some(Some(next)), builtin_next(origin)))
    }

    fn has_realm_prototype(&self, link: Option<Value>, intrinsic: Option<JsObject>) -> bool {
        // A value without a link belongs to the default realm.
        match link {
            None => !self.active_realm_is_extra,
            Some(link) => link.as_object().is_some_and(|link| Some(link) == intrinsic),
        }
    }

    /// The origin of a built-in iterator with no own properties and its
    /// realm's prototype.
    pub(crate) fn plain_iterator_origin(&self, iterator: Value) -> Option<BuiltinIteratorOrigin> {
        let handle = iterator.as_iterator()?;
        let origin = self
            .gc_heap
            .read_payload(handle, IteratorState::builtin_origin)?;
        let plain = self.non_gc_exotic_user_props(&iterator).is_none()
            && self.has_realm_prototype(
                self.non_gc_exotic_prototype_override(&iterator),
                self.active_realm_iterator_prototype_for(origin),
            );
        plain.then_some(origin)
    }

    fn iterable_is_builtin(&mut self, kind: Iterable) -> bool {
        if let Some(builtin) = Proof::current(&self.realm_intrinsics.iteration.iterables[kind as usize])
        {
            return builtin;
        }
        let prototype = move |interp: &Self| match kind {
            Iterable::Array => interp.realm_intrinsics.array_prototype(),
            Iterable::Map => interp.realm_intrinsics.map_prototype(),
            Iterable::Set => interp.realm_intrinsics.set_prototype(),
            Iterable::String => interp.realm_intrinsics.string_prototype(),
        };
        let Some((validity, first)) = self.prove_chain(prototype) else {
            return false;
        };
        let heap = &self.gc_heap;
        let symbol = self.well_known_symbols.get(WellKnown::Iterator);
        let method = chain_data(heap, first, Key::Symbol(symbol));
        let builtin = match kind {
            Iterable::Array => matches!(method, Some(Some(value))
                if value.as_native_function().map(|f| f.raw())
                    == self.realm_intrinsics.array_values().and_then(|v| v.as_native_function()).map(|f| f.raw())),
            Iterable::Map => is_static(heap, method, crate::bootstrap_collections::map_proto_entries),
            Iterable::Set => is_static(heap, method, crate::bootstrap_collections::set_proto_values),
            Iterable::String => is_static(heap, method, crate::string_proto_iterator),
        };
        self.realm_intrinsics.iteration.iterables[kind as usize] = Some(Proof {
            validity,
            facts: builtin,
        });
        builtin
    }

    fn iterator_facts(&mut self, origin: BuiltinIteratorOrigin) -> IteratorFacts {
        let index = origin_index(origin);
        if let Some(facts) = Proof::current(&self.realm_intrinsics.iteration.iterators[index]) {
            return facts;
        }
        let Some((validity, first)) =
            self.prove_chain(move |interp| interp.active_realm_iterator_prototype_for(origin))
        else {
            return IteratorFacts::default();
        };
        let heap = &self.gc_heap;
        let symbol = self.well_known_symbols.get(WellKnown::Iterator);
        let close = chain_data(heap, first, Key::Name("return"));
        let facts = IteratorFacts {
            next: is_static(heap, chain_data(heap, first, Key::Name("next")), builtin_next(origin)),
            close: match builtin_return(origin) {
                Some(expected) => is_static(heap, close, expected),
                None => close == Some(None),
            },
            iterable: is_static(
                heap,
                chain_data(heap, first, Key::Symbol(symbol)),
                crate::intrinsics::iterator::iterator_proto_symbol_iterator,
            ),
        };
        self.realm_intrinsics.iteration.iterators[index] = Some(Proof { validity, facts });
        facts
    }

    /// The validity cell of the chain starting at `prototype`, after giving
    /// every prototype on it the prototype role whose writes retire proofs.
    fn prove_chain(
        &mut self,
        prototype: impl Fn(&Self) -> Option<JsObject>,
    ) -> Option<(Arc<PrototypeValidity>, JsObject)> {
        // Each root may move every prototype, so the chain is re-walked from
        // the realm slot after each one.
        for depth in 0..object::PROTO_CHAIN_HARD_CAP {
            let mut current = prototype(self)?;
            for _ in 0..depth {
                match object::prototype_value(current, &self.gc_heap) {
                    None => {
                        let first = prototype(self)?;
                        return chain_validity(first, &self.gc_heap).map(|cell| (cell, first));
                    }
                    Some(next) => current = next.as_object()?,
                }
            }
            self.object_root(
                Some(current),
                object::DEFAULT_INLINE_CAPACITY,
                ShapeState::ORDINARY,
            )
            .ok()?;
        }
        None
    }
}

/// The record GetIterator returns for a plain built-in iterable: what its
/// built-in `@@iterator` creates.
pub(crate) fn proven_iterator_state(value: Value, heap: &otter_gc::GcHeap) -> Option<IteratorState> {
    if let Some(array) = value.as_array() {
        return Some(IteratorState::Array {
            array,
            index: 0,
            origin: BuiltinIteratorOrigin::Array,
        });
    }
    if let Some(map) = value.as_map() {
        return Some(IteratorState::MapCollection {
            map,
            index: 0,
            kind: MapIteratorKind::Entry,
        });
    }
    if let Some(set) = value.as_set() {
        return Some(IteratorState::SetCollection {
            set,
            index: 0,
            kind: SetIteratorKind::Value,
        });
    }
    value
        .as_string(heap)
        .map(|string| IteratorState::String { string, index: 0 })
}
