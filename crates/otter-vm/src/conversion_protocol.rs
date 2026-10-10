//! The unmodified conversion protocol of ordinary objects.
//!
//! `ToPrimitive` on an object reads `@@toPrimitive`, then `valueOf` and
//! `toString` in hint order, and `%Object.prototype.toString%` reads
//! `@@toStringTag`. While a realm's `%Object.prototype%` holds its built-in
//! `valueOf` and `toString` and neither symbol, an ordinary object whose
//! chain below `%Object.prototype%` shadows none of those four keys converts
//! to `"[object <builtinTag>]"` for every hint without running code:
//! `valueOf` returns the object itself, so `toString` always decides.
//!
//! # Contents
//! - [`ObjectConversionProof`] — per-realm proof over `%Object.prototype%`.
//! - [`Interpreter::unobservable_to_primitive`] — the converted string of a
//!   qualifying object, or `None` for the observable protocol.
//!
//! # Invariants
//! - The proof is the prototype-chain validity cell of `%Object.prototype%`,
//!   taken after reading its answers without running code. Any write to it
//!   retires the cell, and the next conversion reads it again.
//! - Links below `%Object.prototype%` are checked on every conversion: each
//!   is an ordinary object with ordinary named lookup and no own property
//!   under any of the four keys, so every `[[Get]]` the protocol performs
//!   answers from `%Object.prototype%` or finds nothing.
//! - A chain that ends before `%Object.prototype%`, or passes through
//!   anything but an ordinary object, takes the observable protocol.
//!
//! # See also
//! - [`crate::iteration_protocol`] — the same scheme for iteration.
//! - [`crate::regexp_fast`] — the same scheme for RegExp.

use std::sync::Arc;

use crate::object::prototype_validity::{PrototypeValidity, chain_validity};
use crate::object::{self, JsObject, PropertyLookup, ShapeState};
use crate::property_atom::{AtomId, AtomizedPropertyKey, PropertyAtom};
use crate::symbol::WellKnown;
use crate::{Interpreter, JsString, Value, VmError};

const VALUE_OF: &str = "valueOf";
const TO_STRING: &str = "toString";

/// Per-realm proof that `%Object.prototype%` answers the conversion reads
/// with its built-ins, valid while its cell is.
#[derive(Debug, Clone)]
pub(crate) struct ObjectConversionProof {
    validity: Arc<PrototypeValidity>,
    builtin: bool,
    value_of: AtomId,
    to_string: AtomId,
}

/// The interned keys of a proven protocol.
#[derive(Clone, Copy)]
struct ProtocolKeys {
    prototype: JsObject,
    value_of: AtomId,
    to_string: AtomId,
}

impl Interpreter {
    /// `ToPrimitive(input, hint)` when no step of it is observable: the
    /// `"[object <builtinTag>]"` string `%Object.prototype.toString%` would
    /// return. `None` leaves the observable protocol to the caller.
    pub(crate) fn unobservable_to_primitive(
        &mut self,
        input: Value,
    ) -> Result<Option<Value>, VmError> {
        if input.as_object().is_none() {
            return Ok(None);
        }
        let (input, keys) = match self.proven_object_conversion() {
            Some(Some(keys)) => (input, keys),
            Some(None) => return Ok(None),
            None => {
                // Proving may allocate the prototype's root, which can move
                // the receiver.
                let (input, keys) = self.with_handle_scope(|interp, scope| {
                    let root = interp.scoped_value(scope, input);
                    let keys = interp.prove_object_conversion();
                    (interp.escape_scoped(root), keys)
                });
                let Some(keys) = keys else {
                    return Ok(None);
                };
                (input, keys)
            }
        };
        let Some(receiver) = input.as_object() else {
            return Ok(None);
        };
        if !self.chain_reaches_unshadowed(receiver, keys) {
            return Ok(None);
        }
        let tag = crate::object_statics::builtin_to_string_tag_value(input, self);
        let text = JsString::from_str(&format!("[object {tag}]"), &mut self.gc_heap)?;
        Ok(Some(Value::string(text)))
    }

    /// Whether every link from `receiver` up to the proven prototype is an
    /// ordinary object with no own conversion key.
    fn chain_reaches_unshadowed(&self, receiver: JsObject, keys: ProtocolKeys) -> bool {
        let heap = &self.gc_heap;
        let value_of = AtomizedPropertyKey::new(PropertyAtom::new(keys.value_of), VALUE_OF);
        let to_string = AtomizedPropertyKey::new(PropertyAtom::new(keys.to_string), TO_STRING);
        let mut current = receiver;
        for _ in 0..object::PROTO_CHAIN_HARD_CAP {
            if current == keys.prototype {
                return true;
            }
            if object::state(current, heap).is_opaque()
                || !matches!(
                    object::lookup_own_atom(current, heap, value_of).lookup,
                    PropertyLookup::Absent
                )
                || !matches!(
                    object::lookup_own_atom(current, heap, to_string).lookup,
                    PropertyLookup::Absent
                )
                || object::has_own_well_known_symbol(current, heap, WellKnown::ToPrimitive)
                || object::has_own_well_known_symbol(current, heap, WellKnown::ToStringTag)
            {
                return false;
            }
            match object::prototype_value(current, heap).and_then(|next| next.as_object()) {
                Some(next) => current = next,
                None => return false,
            }
        }
        false
    }

    /// The active realm's `%Object.prototype%` with the interned conversion
    /// keys while it is proven to answer them with its built-ins, `Some(None)`
    /// while proven not to, and `None` when no proof is current.
    fn proven_object_conversion(&self) -> Option<Option<ProtocolKeys>> {
        let proof = self.realm_intrinsics.object_conversion.as_ref()?;
        if !proof.validity.is_valid() {
            return None;
        }
        Some(proof.builtin.then(|| {
            ProtocolKeys {
                prototype: self
                    .realm_intrinsics
                    .object_prototype()
                    .unwrap_or_else(JsObject::null),
                value_of: proof.value_of,
                to_string: proof.to_string,
            }
        }))
    }

    /// Read `%Object.prototype%`'s conversion answers into a fresh proof,
    /// returning its keys when they are the built-ins. May allocate the
    /// prototype's instance root once.
    fn prove_object_conversion(&mut self) -> Option<ProtocolKeys> {
        let prototype = self.realm_intrinsics.object_prototype()?;
        // An instance root gives the prototype its prototype role, whose
        // writes retire the proofs taken over it.
        self.object_root(
            Some(prototype),
            object::DEFAULT_INLINE_CAPACITY,
            ShapeState::ORDINARY,
        )
        .ok()?;
        let prototype = self.realm_intrinsics.object_prototype()?;
        let validity = chain_validity(prototype, &self.gc_heap)?;
        let names = self.shape_runtime.names_mut();
        let value_of = names.intern(VALUE_OF);
        let to_string = names.intern(TO_STRING);
        let heap = &self.gc_heap;
        let builtin_method =
            |name: &str, atom: AtomId, expected: crate::native_function::NativeFastFn| {
                let key = AtomizedPropertyKey::new(PropertyAtom::new(atom), name);
                matches!(object::lookup_own_atom(prototype, heap, key).lookup,
                PropertyLookup::Data { value, .. }
                    if value.as_native_function().is_some_and(|f| f.is_static_fn(heap, expected)))
            };
        let builtin = !object::state(prototype, heap).is_opaque()
            && builtin_method(
                VALUE_OF,
                value_of,
                crate::object_statics::native_prototype_value_of,
            )
            && builtin_method(
                TO_STRING,
                to_string,
                crate::object_statics::native_prototype_to_string,
            )
            && !object::has_own_well_known_symbol(prototype, heap, WellKnown::ToPrimitive)
            && !object::has_own_well_known_symbol(prototype, heap, WellKnown::ToStringTag)
            && object::prototype_value(prototype, heap).is_none();
        self.realm_intrinsics.object_conversion = Some(ObjectConversionProof {
            validity,
            builtin,
            value_of,
            to_string,
        });
        builtin.then_some(ProtocolKeys {
            prototype,
            value_of,
            to_string,
        })
    }
}
