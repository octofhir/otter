//! Internal-method result types.
//!
//! These enums report the outcome of `[[GetOwnProperty]]`-style
//! probes ([`PropertyLookup`]) and the dispatch action the
//! interpreter must take after `[[Set]]` resolution
//! ([`SetOutcome`] / [`SetRejectReason`]).
//!
//! # Contents
//! - [`PropertyLookup`] — own-property probe result.
//! - [`SetOutcome`] — `[[Set]]` resolution kind.
//! - [`SetRejectReason`] — stable enum mirroring the spec's reject
//!   reasons (non-writable / accessor-without-setter / non-extensible).
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-ordinarygetownproperty>
//! - <https://tc39.es/ecma262/#sec-ordinaryset>

use crate::Value;

use super::PropertyFlags;

/// Result of an own-property probe.
#[derive(Debug, Clone)]
pub enum PropertyLookup {
    /// No own property of that key exists.
    Absent,
    /// Data property — the stored value plus its attribute flags.
    Data {
        /// Stored value.
        value: Value,
        /// Attribute flags.
        flags: PropertyFlags,
    },
    /// Accessor property.
    Accessor {
        /// `[[Get]]` slot, if any.
        getter: Option<Value>,
        /// `[[Set]]` slot, if any.
        setter: Option<Value>,
        /// Attribute flags. The writable bit is meaningless here.
        flags: PropertyFlags,
    },
}

/// What the runtime should do after `[[Set]]` resolves through the
/// prototype chain (§10.1.9 OrdinarySet).
#[derive(Debug, Clone)]
pub enum SetOutcome {
    /// The own / inherited slot is a writable data slot. The runtime
    /// should write `value` into the receiver as a data property.
    AssignData,
    /// An accessor with a setter was found. The runtime should call
    /// `setter(value)` with `this = receiver`.
    InvokeSetter {
        /// The setter callable.
        setter: Value,
    },
    /// The set must be rejected — non-writable data, accessor with no
    /// setter, or the receiver is non-extensible and the property is
    /// missing. In sloppy mode this is silently dropped; in strict
    /// mode it would surface as a `TypeError`.
    Reject {
        /// Stable rejection reason (used by future strict-mode wiring).
        reason: SetRejectReason,
    },
    /// The walk reached a prototype that is not an ordinary
    /// `JsObject` (a TypedArray, Proxy value, or other exotic). The
    /// runtime must continue the §10.1.9 OrdinarySet walk by
    /// dispatching `parent.[[Set]]` through the value-level funnel —
    /// exotic [[Set]] overrides (e.g. §10.4.5.5) are observable.
    ExoticParent {
        /// The non-ordinary prototype value.
        parent: Value,
    },
}

/// Why a `[[Set]]` was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SetRejectReason {
    /// Existing data property is non-writable.
    NonWritable,
    /// Accessor descriptor has no `[[Set]]`.
    AccessorWithoutSetter,
    /// Receiver is non-extensible and the property is absent.
    NonExtensible,
}
