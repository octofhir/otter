//! ECMA-262 §6.2.5 Property Descriptor types.
//!
//! Two-tier descriptor representation mirrors V8 / JSC / SpiderMonkey:
//!
//! - [`PropertyFlags`] — packed `(writable, enumerable, configurable)`
//!   bitfield used in every stored slot.
//! - [`PropertyDescriptor`] / [`DescriptorKind`] — fully-specified
//!   descriptor (every attribute carries a value). Used for storage,
//!   `FromPropertyDescriptor` output, and accessor / data interop.
//! - [`PartialPropertyDescriptor`] — `Option<…>` per field, the
//!   spec's user-input shape. Returned by `ToPropertyDescriptor`,
//!   consumed by `ValidateAndApplyPropertyDescriptor` /
//!   `[[DefineOwnProperty]]`.
//!
//! # Contents
//! - [`PropertyFlags`] — bit-packed attribute bag.
//! - [`PropertyDescriptor`] / [`DescriptorKind`] — full descriptor.
//! - [`PartialPropertyDescriptor`] — field-presence descriptor.
//!
//! # Invariants
//! - `[[Writable]]` is meaningful only on `DescriptorKind::Data`.
//! - `complete_for_new_property` applies §10.1.6.3 step 5 defaults
//!   when a partial descriptor reaches storage.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-property-descriptor-specification-type>
//! - <https://tc39.es/ecma262/#sec-topropertydescriptor>
//! - <https://tc39.es/ecma262/#sec-validateandapplypropertydescriptor>
//! - <https://tc39.es/ecma262/#table-default-attributes>

use crate::Value;

/// Packed `(writable, enumerable, configurable)` bitfield. Stored as
/// a single byte alongside each slot.
///
/// # See also
/// - <https://tc39.es/ecma262/#table-default-attributes>
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct PropertyFlags(u8);

impl PropertyFlags {
    /// `[[Writable]]` bit.
    pub const WRITABLE: u8 = 0b0001;
    /// `[[Enumerable]]` bit.
    pub const ENUMERABLE: u8 = 0b0010;
    /// `[[Configurable]]` bit.
    pub const CONFIGURABLE: u8 = 0b0100;

    /// All three attributes set — the default for an object-literal
    /// data property created by source like `{ x: 1 }`.
    #[must_use]
    pub const fn data_default() -> Self {
        Self(Self::WRITABLE | Self::ENUMERABLE | Self::CONFIGURABLE)
    }

    /// Every attribute clear — the default `Object.defineProperty`
    /// shape per §10.1.6.3 (`writable / enumerable / configurable`
    /// each default to `false` when absent from the supplied
    /// descriptor).
    #[must_use]
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Build flags from individual bits.
    #[must_use]
    pub const fn new(writable: bool, enumerable: bool, configurable: bool) -> Self {
        let mut bits = 0u8;
        if writable {
            bits |= Self::WRITABLE;
        }
        if enumerable {
            bits |= Self::ENUMERABLE;
        }
        if configurable {
            bits |= Self::CONFIGURABLE;
        }
        Self(bits)
    }

    /// `true` when the `[[Writable]]` bit is set.
    #[must_use]
    pub const fn writable(self) -> bool {
        self.0 & Self::WRITABLE != 0
    }

    /// `true` when the `[[Enumerable]]` bit is set.
    #[must_use]
    pub const fn enumerable(self) -> bool {
        self.0 & Self::ENUMERABLE != 0
    }

    /// `true` when the `[[Configurable]]` bit is set.
    #[must_use]
    pub const fn configurable(self) -> bool {
        self.0 & Self::CONFIGURABLE != 0
    }

    /// Build a fresh value with `[[Writable]]` overridden.
    #[must_use]
    pub fn with_writable(mut self, value: bool) -> Self {
        if value {
            self.0 |= Self::WRITABLE;
        } else {
            self.0 &= !Self::WRITABLE;
        }
        self
    }

    /// Build a fresh value with `[[Enumerable]]` overridden.
    #[must_use]
    pub fn with_enumerable(mut self, value: bool) -> Self {
        if value {
            self.0 |= Self::ENUMERABLE;
        } else {
            self.0 &= !Self::ENUMERABLE;
        }
        self
    }

    /// Build a fresh value with `[[Configurable]]` overridden.
    #[must_use]
    pub fn with_configurable(mut self, value: bool) -> Self {
        if value {
            self.0 |= Self::CONFIGURABLE;
        } else {
            self.0 &= !Self::CONFIGURABLE;
        }
        self
    }
}

/// One property descriptor — either a data property with a stored
/// value or an accessor pair.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-property-descriptor-specification-type>
#[derive(Debug, Clone)]
pub struct PropertyDescriptor {
    /// Body — data slot or accessor pair.
    pub kind: DescriptorKind,
    /// Attribute flags. The `[[Writable]]` bit is meaningful only
    /// when [`kind`](Self::kind) is [`DescriptorKind::Data`]; for
    /// accessors it is ignored.
    pub flags: PropertyFlags,
}

/// Body of a [`PropertyDescriptor`].
#[derive(Debug, Clone)]
pub enum DescriptorKind {
    /// Data property — stores the value directly.
    Data {
        /// Stored value.
        value: Value,
    },
    /// Accessor property — the runtime invokes the relevant function
    /// on read (`getter`) and write (`setter`).
    Accessor {
        /// `Some(callable)` for a `[[Get]]` slot, `None` when absent.
        getter: Option<Value>,
        /// `Some(callable)` for a `[[Set]]` slot, `None` when absent.
        setter: Option<Value>,
    },
}

impl PropertyDescriptor {
    /// Build a data descriptor.
    #[must_use]
    pub fn data(value: Value, writable: bool, enumerable: bool, configurable: bool) -> Self {
        Self {
            kind: DescriptorKind::Data { value },
            flags: PropertyFlags::new(writable, enumerable, configurable),
        }
    }

    /// Build an accessor descriptor.
    #[must_use]
    pub fn accessor(
        getter: Option<Value>,
        setter: Option<Value>,
        enumerable: bool,
        configurable: bool,
    ) -> Self {
        Self {
            kind: DescriptorKind::Accessor { getter, setter },
            // accessor flags never carry the writable bit
            flags: PropertyFlags::new(false, enumerable, configurable),
        }
    }

    /// `true` when this is a data descriptor.
    #[must_use]
    pub fn is_data(&self) -> bool {
        matches!(self.kind, DescriptorKind::Data { .. })
    }

    /// `true` when this is an accessor descriptor.
    #[must_use]
    pub fn is_accessor(&self) -> bool {
        matches!(self.kind, DescriptorKind::Accessor { .. })
    }

    /// Convenience: `[[Configurable]]` bit.
    #[must_use]
    pub fn configurable(&self) -> bool {
        self.flags.configurable()
    }

    /// Convenience: `[[Enumerable]]` bit.
    #[must_use]
    pub fn enumerable(&self) -> bool {
        self.flags.enumerable()
    }

    /// Convenience: `[[Writable]]` bit (meaningful only on data
    /// descriptors).
    #[must_use]
    pub fn writable(&self) -> bool {
        self.flags.writable()
    }
}

/// ECMA-262 §6.2.5 Property Descriptor — the **partial** form used by
/// `ToPropertyDescriptor` and `[[DefineOwnProperty]]` callers.
///
/// Every field is optional and tracks **field presence** so the spec
/// algorithms can distinguish "absent" from "present with value
/// `false`". This is the same slot layout V8 / JSC / SpiderMonkey use
/// for the user-facing descriptor type:
///
/// - V8 `PropertyDescriptor` (`property-descriptor.h`)
/// - JSC `PropertyDescriptor` (`PropertyDescriptor.h`)
/// - SpiderMonkey `Rooted<PropertyDescriptor>` (`PropertyDescriptor.h`)
///
/// Once a partial descriptor reaches storage (the JsObject property
/// slot table), it is completed into a [`PropertyDescriptor`] with
/// spec-mandated default values per §10.1.6.3
/// `ValidateAndApplyPropertyDescriptor` step 5.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-property-descriptor-specification-type>
/// - <https://tc39.es/ecma262/#sec-topropertydescriptor>
/// - <https://tc39.es/ecma262/#sec-validateandapplypropertydescriptor>
#[derive(Debug, Clone, Default)]
pub struct PartialPropertyDescriptor {
    /// `[[Value]]` slot (data descriptor field).
    pub value: Option<Value>,
    /// `[[Writable]]` slot (data descriptor field).
    pub writable: Option<bool>,
    /// `[[Get]]` slot (accessor descriptor field).
    pub get: Option<Value>,
    /// `[[Set]]` slot (accessor descriptor field).
    pub set: Option<Value>,
    /// `[[Enumerable]]` slot.
    pub enumerable: Option<bool>,
    /// `[[Configurable]]` slot.
    pub configurable: Option<bool>,
}

impl PartialPropertyDescriptor {
    /// `true` when the descriptor names any of `[[Value]]` or
    /// `[[Writable]]` (i.e. fits §6.2.5 IsDataDescriptor).
    #[must_use]
    pub fn is_data(&self) -> bool {
        self.value.is_some() || self.writable.is_some()
    }

    /// `true` when the descriptor names any of `[[Get]]` or `[[Set]]`
    /// (§6.2.5 IsAccessorDescriptor).
    #[must_use]
    pub fn is_accessor(&self) -> bool {
        self.get.is_some() || self.set.is_some()
    }

    /// `true` when neither data nor accessor fields are present
    /// (§6.2.5 IsGenericDescriptor).
    #[must_use]
    pub fn is_generic(&self) -> bool {
        !self.is_data() && !self.is_accessor()
    }

    /// Complete the partial descriptor with §10.1.6.3 step 5 defaults
    /// for a newly created property. If the descriptor names accessor
    /// fields the result is an [`DescriptorKind::Accessor`]; otherwise
    /// it is an [`DescriptorKind::Data`].
    #[must_use]
    pub fn complete_for_new_property(&self) -> PropertyDescriptor {
        if self.is_accessor() {
            PropertyDescriptor::accessor(
                self.get.clone(),
                self.set.clone(),
                self.enumerable.unwrap_or(false),
                self.configurable.unwrap_or(false),
            )
        } else {
            PropertyDescriptor::data(
                self.value.clone().unwrap_or(Value::Undefined),
                self.writable.unwrap_or(false),
                self.enumerable.unwrap_or(false),
                self.configurable.unwrap_or(false),
            )
        }
    }

    /// Build a partial descriptor that fully describes `desc` (every
    /// field is present). Used when converting a stored
    /// [`PropertyDescriptor`] back to the user-facing form.
    #[must_use]
    pub fn from_full(desc: &PropertyDescriptor) -> Self {
        match &desc.kind {
            DescriptorKind::Data { value } => Self {
                value: Some(value.clone()),
                writable: Some(desc.flags.writable()),
                get: None,
                set: None,
                enumerable: Some(desc.flags.enumerable()),
                configurable: Some(desc.flags.configurable()),
            },
            DescriptorKind::Accessor { getter, setter } => Self {
                value: None,
                writable: None,
                get: getter.clone(),
                set: setter.clone(),
                enumerable: Some(desc.flags.enumerable()),
                configurable: Some(desc.flags.configurable()),
            },
        }
    }
}
