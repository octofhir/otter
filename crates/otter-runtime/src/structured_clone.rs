//! Structured-clone payloads for worker and isolate boundaries.
//!
//! Worker messages cross isolate boundaries as owned data, never as VM
//! values or GC handles. This module defines the sendable payload shape
//! and the isolate-side clone helper that turns supported VM values
//! into that shape.
//!
//! # Contents
//!
//! - [`StructuredCloneValue`] — owned, sendable clone payload.
//! - [`StructuredCloneTransferList`] — owned transfer-list metadata.
//! - [`StructuredCloneError`] — deterministic failure surface for
//!   unsupported values, cycles, and depth limits.
//! - VM-to-payload cloning helpers used by future worker APIs.
//!
//! # Invariants
//!
//! - Public clone payloads contain no `otter_vm::Value`,
//!   `otter_gc::Gc<T>`, `Local<'gc, T>`, or borrowed VM state.
//! - VM heap access is explicit: clone helpers take `&GcHeap`.
//! - Recursive traversal is depth-limited and tracks the active GC
//!   handle stack so cyclic input fails predictably instead of
//!   recursing without bound.
//!
//! # See also
//!
//! - [Event loop](../../../docs/book/src/engine/event-loop.md)
//! - [Runtime architecture](../../../docs/book/src/engine/architecture.md)

use std::collections::HashSet;

use otter_gc::GcHeap;
use otter_gc::raw::RawGc;
use otter_vm::error_classes::ErrorKind;
use otter_vm::number::NumberValue;
use otter_vm::{Value, array, collections, object};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Default structured-clone recursion limit.
pub const DEFAULT_STRUCTURED_CLONE_MAX_DEPTH: usize = 512;

/// Configuration for VM-to-payload structured cloning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StructuredCloneOptions {
    /// Maximum nested container depth accepted by the clone walker.
    pub max_depth: usize,
}

impl Default for StructuredCloneOptions {
    fn default() -> Self {
        Self {
            max_depth: DEFAULT_STRUCTURED_CLONE_MAX_DEPTH,
        }
    }
}

/// A JS number stored by exact IEEE-754 bit pattern.
///
/// Structured clone must preserve `-0`, infinities, and NaN payload
/// shape well enough that worker boundaries do not silently normalize
/// them through JSON.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredCloneNumber {
    bits: u64,
}

impl StructuredCloneNumber {
    /// Store `value` by its raw IEEE-754 bits.
    #[must_use]
    pub fn from_f64(value: f64) -> Self {
        Self {
            bits: value.to_bits(),
        }
    }

    /// Reconstruct the original `f64`.
    #[must_use]
    pub fn as_f64(self) -> f64 {
        f64::from_bits(self.bits)
    }

    /// Raw IEEE-754 bit pattern.
    #[must_use]
    pub const fn bits(self) -> u64 {
        self.bits
    }
}

impl From<NumberValue> for StructuredCloneNumber {
    fn from(value: NumberValue) -> Self {
        Self::from_f64(value.as_f64())
    }
}

/// String-keyed object property in insertion order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredCloneProperty {
    /// Property key.
    pub key: String,
    /// Owned cloned value.
    pub value: StructuredCloneValue,
}

/// Ordered `Map` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredCloneMapEntry {
    /// Owned cloned key.
    pub key: StructuredCloneValue,
    /// Owned cloned value.
    pub value: StructuredCloneValue,
}

/// Stable id for a future transferable backing resource.
///
/// The id is host-assigned metadata, not a pointer and not a VM/GC
/// handle. Message-port and ArrayBuffer transfer plumbing can use
/// this before the JS-visible worker API exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StructuredCloneTransferId(u64);

impl StructuredCloneTransferId {
    /// Build a transfer id from host-owned metadata.
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    /// Numeric id.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Transferable backing-resource kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StructuredCloneTransferKind {
    /// Future ArrayBuffer backing-store transfer.
    ArrayBuffer,
    /// Future message-port endpoint transfer.
    MessagePort,
    /// Future stream/resource transfer.
    Stream,
}

/// One transfer-list entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct StructuredCloneTransfer {
    /// Host-assigned resource id.
    pub id: StructuredCloneTransferId,
    /// Resource kind.
    pub kind: StructuredCloneTransferKind,
}

impl StructuredCloneTransfer {
    /// Future ArrayBuffer transfer entry.
    #[must_use]
    pub const fn array_buffer(id: StructuredCloneTransferId) -> Self {
        Self {
            id,
            kind: StructuredCloneTransferKind::ArrayBuffer,
        }
    }

    /// Future MessagePort transfer entry.
    #[must_use]
    pub const fn message_port(id: StructuredCloneTransferId) -> Self {
        Self {
            id,
            kind: StructuredCloneTransferKind::MessagePort,
        }
    }

    /// Future stream/resource transfer entry.
    #[must_use]
    pub const fn stream(id: StructuredCloneTransferId) -> Self {
        Self {
            id,
            kind: StructuredCloneTransferKind::Stream,
        }
    }
}

/// Owned transfer-list metadata.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StructuredCloneTransferList {
    transfers: Vec<StructuredCloneTransfer>,
}

impl StructuredCloneTransferList {
    /// Build and validate a transfer list.
    ///
    /// # Errors
    /// Returns [`StructuredCloneTransferListError::Duplicate`] when
    /// the same resource id appears more than once.
    pub fn new(
        transfers: Vec<StructuredCloneTransfer>,
    ) -> Result<Self, StructuredCloneTransferListError> {
        let list = Self { transfers };
        list.validate()?;
        Ok(list)
    }

    /// Empty transfer list.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            transfers: Vec::new(),
        }
    }

    /// Borrow transfers.
    #[must_use]
    pub fn transfers(&self) -> &[StructuredCloneTransfer] {
        &self.transfers
    }

    /// Transfer count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.transfers.len()
    }

    /// `true` when there are no transfer entries.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.transfers.is_empty()
    }

    /// Validate uniqueness.
    ///
    /// # Errors
    /// Returns [`StructuredCloneTransferListError::Duplicate`] when
    /// the same id appears more than once.
    pub fn validate(&self) -> Result<(), StructuredCloneTransferListError> {
        let mut seen = HashSet::with_capacity(self.transfers.len());
        for transfer in &self.transfers {
            if !seen.insert(transfer.id) {
                return Err(StructuredCloneTransferListError::Duplicate {
                    id: transfer.id,
                    transfer_kind: transfer.kind,
                });
            }
        }
        Ok(())
    }
}

/// Owned payload that may cross a worker / isolate boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum StructuredCloneValue {
    /// JS `undefined`.
    Undefined,
    /// JS `null`.
    Null,
    /// JS boolean.
    Boolean(bool),
    /// JS number.
    Number(StructuredCloneNumber),
    /// JS BigInt, decimal string form.
    BigInt(String),
    /// JS string.
    String(String),
    /// JS array.
    Array(Vec<StructuredCloneValue>),
    /// Plain object, string-keyed enumerable data properties only.
    Object(Vec<StructuredCloneProperty>),
    /// JS `Map`, insertion order preserved.
    Map(Vec<StructuredCloneMapEntry>),
    /// JS `Set`, insertion order preserved.
    Set(Vec<StructuredCloneValue>),
    /// Error-like diagnostic payload.
    Error {
        /// Error class/name.
        name: String,
        /// Error message.
        message: String,
        /// Optional stack string when the source engine provides one.
        stack: Option<String>,
    },
}

/// Structured-clone failure.
#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StructuredCloneError {
    /// The input contains a value kind this slice does not clone.
    #[error("structured clone unsupported value at {path}: {type_name}")]
    UnsupportedValue {
        /// Deterministic path in the input graph.
        path: String,
        /// VM value kind.
        type_name: &'static str,
    },
    /// The input is deeper than the configured limit.
    #[error("structured clone depth limit {limit} exceeded at {path}")]
    DepthLimitExceeded {
        /// Deterministic path in the input graph.
        path: String,
        /// Configured maximum depth.
        limit: usize,
    },
    /// The input graph contains a cycle.
    #[error("structured clone cycle detected at {path}")]
    Cycle {
        /// Deterministic path in the input graph.
        path: String,
    },
}

/// Transfer-list validation failure.
#[derive(Debug, Clone, PartialEq, Eq, Error, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StructuredCloneTransferListError {
    /// The same transferable resource appears more than once.
    #[error("duplicate structured-clone transfer id {} ({transfer_kind:?})", .id.get())]
    Duplicate {
        /// Duplicate resource id.
        id: StructuredCloneTransferId,
        /// Duplicate resource kind.
        transfer_kind: StructuredCloneTransferKind,
    },
}

/// Clone a VM value into an owned worker-boundary payload.
///
/// This stays crate-visible until the worker API lands; exposing it
/// publicly would leak the internal VM `Value` type through the
/// runtime boundary.
#[allow(
    dead_code,
    reason = "worker postMessage plumbing will call this VM-to-owned-payload boundary"
)]
pub(crate) fn clone_vm_value(
    value: &Value,
    heap: &GcHeap,
) -> Result<StructuredCloneValue, StructuredCloneError> {
    clone_vm_value_with_options(value, heap, StructuredCloneOptions::default())
}

#[allow(
    dead_code,
    reason = "worker postMessage plumbing will call this VM-to-owned-payload boundary"
)]
pub(crate) fn clone_vm_value_with_options(
    value: &Value,
    heap: &GcHeap,
    options: StructuredCloneOptions,
) -> Result<StructuredCloneValue, StructuredCloneError> {
    let mut active = HashSet::new();
    clone_value(value, heap, &options, 0, "$".to_string(), &mut active)
}

fn clone_value(
    value: &Value,
    heap: &GcHeap,
    options: &StructuredCloneOptions,
    depth: usize,
    path: String,
    active: &mut HashSet<RawGc>,
) -> Result<StructuredCloneValue, StructuredCloneError> {
    if depth > options.max_depth {
        return Err(StructuredCloneError::DepthLimitExceeded {
            path,
            limit: options.max_depth,
        });
    }

    match value {
        Value::Undefined => Ok(StructuredCloneValue::Undefined),
        Value::Null => Ok(StructuredCloneValue::Null),
        Value::Boolean(value) => Ok(StructuredCloneValue::Boolean(*value)),
        Value::Number(value) => Ok(StructuredCloneValue::Number((*value).into())),
        Value::BigInt(value) => Ok(StructuredCloneValue::BigInt(value.to_decimal_string(heap))),
        Value::String(value) => Ok(StructuredCloneValue::String(value.to_lossy_string(heap))),
        Value::Array(array) => clone_array(*array, heap, options, depth, path, active),
        Value::Object(object) => clone_object(*object, heap, options, depth, path, active),
        Value::Map(map) => clone_map(*map, heap, options, depth, path, active),
        Value::Set(set) => clone_set(*set, heap, options, depth, path, active),
        other => Err(StructuredCloneError::UnsupportedValue {
            path,
            type_name: value_type_name(other),
        }),
    }
}

fn clone_array(
    array: otter_vm::array::JsArray,
    heap: &GcHeap,
    options: &StructuredCloneOptions,
    depth: usize,
    path: String,
    active: &mut HashSet<RawGc>,
) -> Result<StructuredCloneValue, StructuredCloneError> {
    enter_container(array.raw(), &path, active)?;
    let len = array::len(array, heap);
    let values: Vec<Value> = (0..len).map(|idx| array::get(array, heap, idx)).collect();
    let mut cloned = Vec::with_capacity(values.len());
    for (idx, value) in values.iter().enumerate() {
        cloned.push(clone_value(
            value,
            heap,
            options,
            depth + 1,
            format!("{path}[{idx}]"),
            active,
        )?);
    }
    active.remove(&array.raw());
    Ok(StructuredCloneValue::Array(cloned))
}

fn clone_object(
    object: otter_vm::object::JsObject,
    heap: &GcHeap,
    options: &StructuredCloneOptions,
    depth: usize,
    path: String,
    active: &mut HashSet<RawGc>,
) -> Result<StructuredCloneValue, StructuredCloneError> {
    if let Some(error) = clone_error_object(object, heap) {
        return Ok(error);
    }
    enter_container(object.raw(), &path, active)?;
    let properties: Vec<(String, Value)> = object::with_properties(object, heap, |properties| {
        properties
            .enumerable_data_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect()
    });
    let mut cloned = Vec::with_capacity(properties.len());
    for (key, value) in properties {
        let child_path = object_property_path(&path, &key);
        cloned.push(StructuredCloneProperty {
            key,
            value: clone_value(&value, heap, options, depth + 1, child_path, active)?,
        });
    }
    active.remove(&object.raw());
    Ok(StructuredCloneValue::Object(cloned))
}

fn clone_error_object(
    object: otter_vm::object::JsObject,
    heap: &GcHeap,
) -> Option<StructuredCloneValue> {
    let name = match object::get(object, heap, "name") {
        Some(Value::String(value)) => value.to_lossy_string(heap),
        Some(value) => value.display_string(heap),
        None => return None,
    };
    ErrorKind::from_class_name(&name)?;
    let message = match object::get(object, heap, "message") {
        Some(Value::String(value)) => value.to_lossy_string(heap),
        Some(Value::Undefined) | None => String::new(),
        Some(value) => value.display_string(heap),
    };
    let stack = match object::get(object, heap, "stack") {
        Some(Value::String(value)) => Some(value.to_lossy_string(heap)),
        Some(Value::Undefined) | None => None,
        Some(value) => Some(value.display_string(heap)),
    };
    Some(StructuredCloneValue::Error {
        name,
        message,
        stack,
    })
}

fn clone_map(
    map: otter_vm::collections::JsMap,
    heap: &GcHeap,
    options: &StructuredCloneOptions,
    depth: usize,
    path: String,
    active: &mut HashSet<RawGc>,
) -> Result<StructuredCloneValue, StructuredCloneError> {
    enter_container(map.raw(), &path, active)?;
    let entries = collections::map_entries(map, heap);
    let mut cloned = Vec::with_capacity(entries.len());
    for (idx, (key, value)) in entries.iter().enumerate() {
        cloned.push(StructuredCloneMapEntry {
            key: clone_value(
                key,
                heap,
                options,
                depth + 1,
                format!("{path}<map-key:{idx}>"),
                active,
            )?,
            value: clone_value(
                value,
                heap,
                options,
                depth + 1,
                format!("{path}<map-value:{idx}>"),
                active,
            )?,
        });
    }
    active.remove(&map.raw());
    Ok(StructuredCloneValue::Map(cloned))
}

fn clone_set(
    set: otter_vm::collections::JsSet,
    heap: &GcHeap,
    options: &StructuredCloneOptions,
    depth: usize,
    path: String,
    active: &mut HashSet<RawGc>,
) -> Result<StructuredCloneValue, StructuredCloneError> {
    enter_container(set.raw(), &path, active)?;
    let values = collections::set_values(set, heap);
    let mut cloned = Vec::with_capacity(values.len());
    for (idx, value) in values.iter().enumerate() {
        cloned.push(clone_value(
            value,
            heap,
            options,
            depth + 1,
            format!("{path}<set-value:{idx}>"),
            active,
        )?);
    }
    active.remove(&set.raw());
    Ok(StructuredCloneValue::Set(cloned))
}

fn enter_container(
    raw: RawGc,
    path: &str,
    active: &mut HashSet<RawGc>,
) -> Result<(), StructuredCloneError> {
    if !active.insert(raw) {
        return Err(StructuredCloneError::Cycle {
            path: path.to_string(),
        });
    }
    Ok(())
}

fn object_property_path(base: &str, key: &str) -> String {
    if is_identifier_path_segment(key) {
        format!("{base}.{key}")
    } else {
        format!("{base}[{key:?}]")
    }
}

fn is_identifier_path_segment(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first == '$' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch == '$' || ch.is_ascii_alphanumeric())
}

fn value_type_name(value: &Value) -> &'static str {
    match value {
        Value::Symbol(_) => "symbol",
        Value::Function { .. } => "function",
        Value::Closure(_) => "closure",
        Value::BoundFunction(_) => "bound_function",
        Value::NativeFunction(_) => "native_function",
        Value::Iterator(_) => "iterator",
        Value::RegExp(_) => "regexp",
        Value::Promise(_) => "promise",
        Value::WeakMap(_) => "weak_map",
        Value::WeakSet(_) => "weak_set",
        Value::WeakRef(_) => "weak_ref",
        Value::FinalizationRegistry(_) => "finalization_registry",
        Value::Temporal(_) => "temporal",
        Value::Intl(_) => "intl",
        Value::ArrayBuffer(_) => "array_buffer",
        Value::DataView(_) => "data_view",
        Value::TypedArray(_) => "typed_array",
        Value::Generator(_) => "generator",
        Value::Proxy(_) => "proxy",
        Value::ClassConstructor(_) => "class_constructor",
        Value::Undefined
        | Value::Hole
        | Value::Null
        | Value::Boolean(_)
        | Value::Number(_)
        | Value::BigInt(_)
        | Value::String(_)
        | Value::Object(_)
        | Value::Array(_)
        | Value::Map(_)
        | Value::Set(_) => "supported",
    }
}

#[cfg(test)]
mod tests {
    use otter_vm::{ExecutionContext, Interpreter};

    use super::*;

    fn assert_send_sync_static<T: Send + Sync + 'static>() {}

    fn clone_js_value(source: &str) -> Result<StructuredCloneValue, StructuredCloneError> {
        clone_js_value_with_options(source, StructuredCloneOptions::default())
    }

    fn clone_js_value_with_options(
        source: &str,
        options: StructuredCloneOptions,
    ) -> Result<StructuredCloneValue, StructuredCloneError> {
        let compiled = otter_compiler::compile_script_source_to_module(
            source,
            otter_syntax::SourceKind::JavaScript,
            "<structured-clone-test>",
        )
        .expect("compile fixture");
        let context = ExecutionContext::from_module(compiled.bytecode);
        let mut interp = Interpreter::new();
        let value = interp.run(&context).expect("run fixture");
        clone_vm_value_with_options(&value, interp.gc_heap(), options)
    }

    #[test]
    fn public_payload_is_send_sync_static_owned_data() {
        assert_send_sync_static::<StructuredCloneValue>();
        assert_send_sync_static::<StructuredCloneError>();
        assert_send_sync_static::<StructuredCloneTransferList>();
        assert_send_sync_static::<StructuredCloneTransferListError>();
    }

    #[test]
    fn clones_owned_primitives_and_collections() {
        let cloned = clone_js_value(
            r#"
            const array = [7, "array"];
            const map = new Map();
            map.set("key", array);
            const set = new Set();
            set.add(true);
            const object = {};
            object.map = map;
            object.set = set;
            object;
            "#,
        )
        .unwrap();

        assert_eq!(
            cloned,
            StructuredCloneValue::Object(vec![
                StructuredCloneProperty {
                    key: "map".to_string(),
                    value: StructuredCloneValue::Map(vec![StructuredCloneMapEntry {
                        key: StructuredCloneValue::String("key".to_string()),
                        value: StructuredCloneValue::Array(vec![
                            StructuredCloneValue::Number(StructuredCloneNumber::from_f64(7.0)),
                            StructuredCloneValue::String("array".to_string()),
                        ]),
                    }]),
                },
                StructuredCloneProperty {
                    key: "set".to_string(),
                    value: StructuredCloneValue::Set(vec![StructuredCloneValue::Boolean(true)]),
                },
            ])
        );
    }

    #[test]
    fn rejects_cycles_with_stable_path() {
        let err = clone_js_value(
            r#"
            const object = {};
            object.self = object;
            object;
            "#,
        )
        .unwrap_err();

        assert_eq!(
            err,
            StructuredCloneError::Cycle {
                path: "$.self".to_string(),
            }
        );
    }

    #[test]
    fn rejects_values_beyond_depth_limit() {
        let err = clone_js_value_with_options("[[null]];", StructuredCloneOptions { max_depth: 0 })
            .unwrap_err();

        assert_eq!(
            err,
            StructuredCloneError::DepthLimitExceeded {
                path: "$[0]".to_string(),
                limit: 0,
            }
        );
    }

    #[test]
    fn rejects_unsupported_values_with_stable_path() {
        let err = clone_js_value("[function unsupported() {}];").unwrap_err();

        assert_eq!(
            err,
            StructuredCloneError::UnsupportedValue {
                path: "$[0]".to_string(),
                type_name: "function",
            }
        );
    }

    #[test]
    fn clones_error_like_objects_as_diagnostics() {
        let cloned = clone_js_value(
            r#"
            const error = {};
            error.name = "TypeError";
            error.message = "bad value";
            error.stack = "TypeError: bad value";
            error;
            "#,
        )
        .unwrap();

        assert_eq!(
            cloned,
            StructuredCloneValue::Error {
                name: "TypeError".to_string(),
                message: "bad value".to_string(),
                stack: Some("TypeError: bad value".to_string()),
            }
        );
    }

    #[test]
    fn transfer_list_rejects_duplicate_resources() {
        let id = StructuredCloneTransferId::new(7);
        let err = StructuredCloneTransferList::new(vec![
            StructuredCloneTransfer::array_buffer(id),
            StructuredCloneTransfer::array_buffer(id),
        ])
        .unwrap_err();

        assert_eq!(
            err,
            StructuredCloneTransferListError::Duplicate {
                id,
                transfer_kind: StructuredCloneTransferKind::ArrayBuffer,
            }
        );
    }
}
