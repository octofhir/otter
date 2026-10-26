//! The unmodified-Promise protocol.
//!
//! The promise operations observe a native promise through `then`,
//! `constructor` and `%Promise%[@@species]`. While a promise and its realm's
//! `%Promise.prototype%` are exactly as bootstrap left them, those reads have
//! known answers, so resolution, `await`, `Promise.resolve` and `then` skip
//! them and the resolving functions they would feed.
//!
//! # Contents
//! - [`PromiseProtocolProof`] — per-realm record that the prototype is built-in.
//! - [`Interpreter::pristine_promise`] — a promise qualifies.
//! - [`Interpreter::intrinsic_promise`] / [`Interpreter::is_intrinsic_promise`]
//!   — the realm's `%Promise%`, never the mutable global binding.
//! - [`Interpreter::is_intrinsic_promise_then`] — `%Promise.prototype.then%`.
//!
//! # Invariants
//! - The proof is the prototype-chain validity cell of `%Promise.prototype%`,
//!   taken after verifying its `then` is `%Promise.prototype.then%` and its
//!   `constructor` is `%Promise%`. Any write to the prototype or its chain
//!   retires the cell, and the next use verifies again.
//! - `%Promise%[@@species]` lives on the constructor, outside that chain, so
//!   it is checked on every use.
//! - A promise qualifies with no own property and no `[[Prototype]]`
//!   override: `Get(p, "then")` and `Get(p, "constructor")` then run no user
//!   code and answer the intrinsics.
//!
//! # See also
//! - [`crate::promise_dispatch`] — the operations that take these proofs.
//! - [`crate::regexp_fast`] — the same proof shape for RegExp.

use std::sync::Arc;

use crate::object::prototype_validity::PrototypeValidity;
use crate::object::{self, ShapeState};
use crate::promise::JsPromiseHandle;
use crate::{Interpreter, Value};

/// Per-realm proof about `%Promise.prototype%`, valid while its cell is.
#[derive(Debug, Clone)]
pub(crate) struct PromiseProtocolProof {
    validity: Arc<PrototypeValidity>,
    builtin: bool,
}

impl Interpreter {
    /// The realm's `%Promise%`.
    pub(crate) fn intrinsic_promise(&self) -> Option<Value> {
        self.realm_intrinsics.promise().map(Value::native_function)
    }

    /// Whether `value` is the realm's `%Promise%`.
    pub(crate) fn is_intrinsic_promise(&self, value: &Value) -> bool {
        value.as_native_function().is_some_and(|native| {
            self.realm_intrinsics
                .promise()
                .is_some_and(|promise| promise.ptr_eq(&native))
        })
    }

    /// Whether `value` is the realm's `%Promise.prototype.then%`.
    pub(crate) fn is_intrinsic_promise_then(&self, value: &Value) -> bool {
        value.as_native_function().is_some_and(|native| {
            self.realm_intrinsics
                .promise_then()
                .is_some_and(|then| then.ptr_eq(&native))
        })
    }

    /// `value` as a native promise whose `then`, `constructor` and species
    /// reads have the built-in answers. May allocate the prototype's instance
    /// root once.
    pub(crate) fn pristine_promise(&mut self, value: Value) -> Option<JsPromiseHandle> {
        let promise = value.as_promise()?;
        let plain = promise.expando(&self.gc_heap).is_none()
            && promise.prototype_override(&self.gc_heap).is_none();
        (plain && self.promise_protocol_is_builtin()).then_some(promise)
    }

    /// Whether `%Promise.prototype%` and `%Promise%[@@species]` are built-in.
    pub(crate) fn promise_protocol_is_builtin(&mut self) -> bool {
        self.promise_prototype_is_builtin() && self.promise_species_is_builtin()
    }

    fn promise_species_is_builtin(&self) -> bool {
        let Some(promise) = self.realm_intrinsics.promise() else {
            return false;
        };
        let heap = &self.gc_heap;
        let species = self
            .well_known_symbols
            .get(crate::symbol::WellKnown::Species);
        promise
            .own_symbol_property_descriptor(heap, species)
            .is_some_and(|descriptor| {
                matches!(
                    descriptor.kind,
                    object::DescriptorKind::Accessor { getter: Some(getter), .. }
                        if getter.as_native_function().is_some_and(|getter| {
                            getter.is_static_fn(
                                heap,
                                crate::bootstrap_promise::promise_species_get,
                            )
                        })
                )
            })
    }

    fn promise_prototype_is_builtin(&mut self) -> bool {
        if let Some(proof) = &self.realm_intrinsics.promise_protocol
            && proof.validity.is_valid()
        {
            return proof.builtin;
        }
        let Some(prototype) = self
            .realm_intrinsics
            .get(crate::realm_intrinsics::Intrinsic::PromisePrototype)
        else {
            return false;
        };
        // An instance root gives the prototype its prototype role, whose
        // writes retire the proofs taken over it.
        if self
            .object_root(
                Some(prototype),
                object::DEFAULT_INLINE_CAPACITY,
                ShapeState::ORDINARY,
            )
            .is_err()
        {
            return false;
        }
        let Some(prototype) = self
            .realm_intrinsics
            .get(crate::realm_intrinsics::Intrinsic::PromisePrototype)
        else {
            return false;
        };
        let Some(validity) = object::prototype_validity::chain_validity(prototype, &self.gc_heap)
        else {
            return false;
        };
        let heap = &self.gc_heap;
        let data = |key: &str| match object::lookup_own(prototype, heap, key) {
            object::PropertyLookup::Data { value, .. } => Some(value),
            _ => None,
        };
        let builtin = data("then").is_some_and(|then| self.is_intrinsic_promise_then(&then))
            && data("constructor").is_some_and(|ctor| self.is_intrinsic_promise(&ctor));
        self.realm_intrinsics.promise_protocol = Some(PromiseProtocolProof { validity, builtin });
        builtin
    }
}
