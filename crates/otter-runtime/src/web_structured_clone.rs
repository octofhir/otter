//! In-realm `structuredClone` on the runtime's collector-traced handle arena.
//!
//! # Contents
//! - [`structured_clone`] — clone one reachable platform-object graph.
//! - [`structured_clone_with_options`] — the same operation with an
//!   ArrayBuffer transfer list.
//!
//! # Invariants
//! - The complete operation runs inside one [`NativeScope`]. Sources, clone
//!   shells, collection entries, backing buffers, and transfer entries are
//!   [`Local`] handles rewritten in place by moving collections.
//! - A clone shell is published in the memo before any child is visited, so
//!   cycles and shared references preserve identity. The memo contains handle
//!   indices, not raw GC offsets, and adds no second root traversal.
//! - Recursion is bounded. Enumerable properties are read through ordinary
//!   JavaScript `[[Get]]`, so getters run in key order and abrupt completion
//!   propagates without detaching transfer entries.
//! - Transfer entries are validated before cloning and revalidated before the
//!   non-reentrant detach commit, preventing partial detachment on failure.
//!
//! # See also
//! - <https://html.spec.whatwg.org/multipage/structured-data.html>

use otter_vm::object::PropertyFlags;
use otter_vm::{Local, NativeCtx, NativeError, NativeScope, Value};

const MAX_STRUCTURED_CLONE_DEPTH: usize = 512;

struct CloneState<'scope> {
    memo: Vec<(Local<'scope>, Local<'scope>)>,
}

impl CloneState<'_> {
    fn new() -> Self {
        Self { memo: Vec::new() }
    }
}

/// `structuredClone(value)` — clone `value` within the current realm.
///
/// # Errors
/// Returns a catchable native error when the graph contains an unsupported
/// value, a getter completes abruptly, or allocation fails.
pub fn structured_clone(ctx: &mut NativeCtx<'_>, value: Value) -> Result<Value, NativeError> {
    structured_clone_with_options(ctx, value, Value::undefined())
}

/// `structuredClone(value, options)` with transactional ArrayBuffer transfer.
pub fn structured_clone_with_options(
    ctx: &mut NativeCtx<'_>,
    value: Value,
    options: Value,
) -> Result<Value, NativeError> {
    ctx.scope(|mut scope| {
        let source = scope.value(value);
        let options = scope.value(options);
        let transfers = transfer_list(&mut scope, options)?;
        let mut state = CloneState::new();
        let cloned = clone_local(&mut scope, source, &mut state, 0)?;

        // Getters executed while cloning may detach a listed buffer. Validate
        // the complete list again before the first irreversible operation.
        validate_transfers(&scope, &transfers)?;
        for transfer in transfers {
            scope.detach_array_buffer(transfer)?;
        }

        Ok(scope.finish(cloned))
    })
}

fn data_clone_error(kind: &str) -> NativeError {
    NativeError::TypeError {
        name: "structuredClone",
        reason: format!("{kind} could not be cloned (DataCloneError)"),
    }
}

fn type_error(message: impl Into<String>) -> NativeError {
    NativeError::TypeError {
        name: "structuredClone",
        reason: message.into(),
    }
}

fn transfer_list<'scope, 'rt>(
    scope: &mut NativeScope<'scope, 'rt>,
    options: Local<'scope>,
) -> Result<Vec<Local<'scope>>, NativeError> {
    if scope.is_undefined(options) || scope.is_null(options) {
        return Ok(Vec::new());
    }
    if !scope.is_object(options) {
        return Err(type_error("options must be an object"));
    }

    let transfer = scope.get(options, "transfer")?;
    if scope.is_undefined(transfer) || scope.is_null(transfer) {
        return Ok(Vec::new());
    }
    if !scope.is_array(transfer)? {
        return Err(type_error("options.transfer must be an Array"));
    }

    let len = scope.array_length(transfer)?;
    let mut transfers = Vec::with_capacity(len);
    for index in 0..len {
        // Use ordinary Get instead of the dense-array reader: transfer arrays
        // may carry indexed accessors whose abrupt completion is observable.
        let item = scope.get(transfer, &index.to_string())?;
        if scope.array_buffer_is_shared(item) != Some(false)
            || scope.array_buffer_is_detached(item) != Some(false)
        {
            return Err(data_clone_error("Transfer item"));
        }
        if transfers
            .iter()
            .copied()
            .any(|seen| scope.strict_equals(seen, item))
        {
            return Err(data_clone_error("ArrayBuffer"));
        }
        transfers.push(item);
    }
    Ok(transfers)
}

fn validate_transfers(
    scope: &NativeScope<'_, '_>,
    transfers: &[Local<'_>],
) -> Result<(), NativeError> {
    for transfer in transfers {
        if scope.array_buffer_is_shared(*transfer) != Some(false)
            || scope.array_buffer_is_detached(*transfer) != Some(false)
        {
            return Err(data_clone_error("ArrayBuffer"));
        }
    }
    Ok(())
}

fn memoized<'scope, 'rt>(
    scope: &NativeScope<'scope, 'rt>,
    state: &CloneState<'scope>,
    source: Local<'scope>,
) -> Option<Local<'scope>> {
    state
        .memo
        .iter()
        .find_map(|(seen, cloned)| scope.strict_equals(*seen, source).then_some(*cloned))
}

fn publish<'scope>(state: &mut CloneState<'scope>, source: Local<'scope>, cloned: Local<'scope>) {
    state.memo.push((source, cloned));
}

fn clone_local<'scope, 'rt>(
    scope: &mut NativeScope<'scope, 'rt>,
    source: Local<'scope>,
    state: &mut CloneState<'scope>,
    depth: usize,
) -> Result<Local<'scope>, NativeError> {
    if depth > MAX_STRUCTURED_CLONE_DEPTH {
        return Err(data_clone_error("Object graph"));
    }

    if scope.is_undefined(source)
        || scope.is_null(source)
        || scope.boolean_value(source).is_ok()
        || scope.number_value(source).is_ok()
        || scope.is_string(source)
        || scope.is_bigint(source)
    {
        return Ok(source);
    }
    if scope.is_symbol(source) {
        return Err(data_clone_error("Symbol"));
    }
    if scope.is_callable(source) {
        return Err(data_clone_error("Function"));
    }
    if let Some(cloned) = memoized(scope, state, source) {
        return Ok(cloned);
    }

    if let Some(detached) = scope.array_buffer_is_detached(source) {
        if detached {
            return Err(data_clone_error("ArrayBuffer"));
        }
        if scope.array_buffer_is_shared(source) == Some(true) {
            let body = scope
                .shared_array_buffer_body(source)
                .ok_or_else(|| data_clone_error("SharedArrayBuffer"))?;
            let cloned = scope.shared_array_buffer(body)?;
            publish(state, source, cloned);
            return Ok(cloned);
        }
        let bytes = scope
            .array_buffer_bytes(source)
            .ok_or_else(|| data_clone_error("ArrayBuffer"))?;
        let cloned = scope.array_buffer_from_bytes(bytes)?;
        publish(state, source, cloned);
        return Ok(cloned);
    }

    if let Some((kind, backing, byte_offset, length)) = scope.typed_array_info(source) {
        let cloned_backing = clone_local(scope, backing, state, depth + 1)?;
        let cloned = scope.typed_array_view(cloned_backing, kind, byte_offset, length)?;
        publish(state, source, cloned);
        return Ok(cloned);
    }

    if let Some((backing, byte_offset, byte_length)) = scope.data_view_info(source) {
        let cloned_backing = clone_local(scope, backing, state, depth + 1)?;
        let cloned = scope.data_view(cloned_backing, byte_offset, byte_length)?;
        publish(state, source, cloned);
        return Ok(cloned);
    }

    if scope.is_exact_array(source) {
        let len = scope.array_length(source)?;
        let cloned = scope.array(len)?;
        publish(state, source, cloned);
        clone_enumerable_properties(scope, source, cloned, true, state, depth)?;
        return Ok(cloned);
    }

    if let Ok(entries) = scope.map_entries(source) {
        let cloned = scope.map_collection()?;
        publish(state, source, cloned);
        for (key, value) in entries {
            let key = clone_local(scope, key, state, depth + 1)?;
            let value = clone_local(scope, value, state, depth + 1)?;
            scope.map_set(cloned, key, value)?;
        }
        return Ok(cloned);
    }

    if let Ok(values) = scope.set_values(source) {
        let cloned = scope.set_collection()?;
        publish(state, source, cloned);
        for value in values {
            let value = clone_local(scope, value, state, depth + 1)?;
            scope.set_add(cloned, value)?;
        }
        return Ok(cloned);
    }

    if let Some(milliseconds) = scope.date_value(source) {
        let cloned = scope.date(milliseconds)?;
        publish(state, source, cloned);
        return Ok(cloned);
    }

    if let Some((pattern, flags, _last_index)) = scope.regexp_snapshot(source) {
        let cloned = scope.regexp(&pattern, &flags)?;
        publish(state, source, cloned);
        return Ok(cloned);
    }

    if scope.has_error_data(source) {
        return clone_error(scope, source, state, depth);
    }

    if scope.is_ordinary_object(source) {
        let cloned = scope.object()?;
        publish(state, source, cloned);
        clone_enumerable_properties(scope, source, cloned, false, state, depth)?;
        return Ok(cloned);
    }

    Err(data_clone_error("value"))
}

fn clone_enumerable_properties<'scope, 'rt>(
    scope: &mut NativeScope<'scope, 'rt>,
    source: Local<'scope>,
    target: Local<'scope>,
    target_is_array: bool,
    state: &mut CloneState<'scope>,
    depth: usize,
) -> Result<(), NativeError> {
    let keys = scope.enumerable_own_string_keys(source)?;
    for key in keys {
        let value = scope.get(source, &key)?;
        let value = clone_local(scope, value, state, depth + 1)?;
        if target_is_array {
            if let Some(index) = array_index(&key) {
                scope.set_index(target, index, value)?;
            } else {
                scope.set(target, &key, value)?;
            }
        } else {
            scope.define(target, &key, value, PropertyFlags::data_default())?;
        }
    }
    Ok(())
}

fn array_index(key: &str) -> Option<usize> {
    let index = key.parse::<u32>().ok()?;
    if index == u32::MAX || index.to_string() != key {
        return None;
    }
    Some(index as usize)
}

fn clone_error<'scope, 'rt>(
    scope: &mut NativeScope<'scope, 'rt>,
    source: Local<'scope>,
    state: &mut CloneState<'scope>,
    depth: usize,
) -> Result<Local<'scope>, NativeError> {
    let name = scope.get(source, "name")?;
    let name = if scope.is_string(name) {
        scope.string_value(name)?
    } else {
        "Error".to_string()
    };
    let kind = otter_vm::error_classes::ErrorKind::from_class_name(&name)
        .unwrap_or(otter_vm::error_classes::ErrorKind::Error);

    let message = scope.get(source, "message")?;
    let message = if scope.is_string(message) {
        scope.string_value(message)?
    } else {
        String::new()
    };
    let cloned = scope.error(kind, &message)?;
    publish(state, source, cloned);

    if scope.has_own_string_property(source, "cause") {
        let cause = scope.get(source, "cause")?;
        let cause = clone_local(scope, cause, state, depth + 1)?;
        scope.define(
            cloned,
            "cause",
            cause,
            PropertyFlags::new(true, false, true),
        )?;
    }
    Ok(cloned)
}
