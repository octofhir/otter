//! Accounted HTTP body transport for `Otter.serve`.
//!
//! `serve.rs` owns HTTP parsing and Fetch object conversion. This module keeps
//! every buffered Rust-side body inseparable from both the runtime's aggregate
//! external-memory charge and a finite per-server charge until Web Streams can
//! replace whole-body buffering.
//!
//! # Contents
//! - [`ServeBodyBudget`] - shared per-server and aggregate-runtime admission.
//! - [`ServeBodyBuilder`] - incremental request-body accumulation.
//! - [`ServeBody`] - request/response bytes passed across serve tasks.
//!
//! # Invariants
//! - A single buffered body retains at most 16 MiB and one server retains at
//!   most 64 MiB of native body bytes, even when the runtime ledger is unlimited.
//! - Runtime and server charges grow before byte capacity and roll back together
//!   after allocation failure or rejection.
//! - Body data crossing async/runtime boundaries is owned and `Send`; no VM
//!   values or raw GC handles are stored in transport state.
//! - Dropping a partial builder, completed body, rejected response, or network
//!   body releases every corresponding charge.
//!
//! # See also
//! - [`crate::serve`]

use std::collections::TryReserveError;

use otter_runtime::{
    ResourceAccount, ResourceClass, ResourceError, ResourceLease, RuntimeJsString as JsString,
    RuntimeNativeCtx as NativeCtx, RuntimeNativeError as NativeError, RuntimeValue as Value,
    runtime_type_error,
};

/// Maximum bytes retained for one buffered request or response body.
pub(crate) const MAX_BUFFERED_SERVE_BODY_BYTES: u64 = 16 * 1024 * 1024;
/// Maximum native body bytes retained concurrently by one server.
const MAX_SERVER_BUFFERED_BODY_BYTES: u64 = 64 * 1024 * 1024;

/// Shared admission state for all buffered bodies owned by one server.
#[derive(Clone)]
pub(crate) struct ServeBodyBudget {
    runtime: ResourceAccount,
    server: ResourceAccount,
    max_body_bytes: u64,
}

impl ServeBodyBudget {
    /// Build the transparent default budget for one `Otter.serve` instance.
    #[must_use]
    pub(crate) fn standard(runtime: ResourceAccount) -> Self {
        Self::with_limits(
            runtime,
            MAX_BUFFERED_SERVE_BODY_BYTES,
            MAX_SERVER_BUFFERED_BODY_BYTES,
        )
    }

    fn with_limits(runtime: ResourceAccount, max_body_bytes: u64, server_bytes: u64) -> Self {
        Self {
            runtime,
            server: ResourceAccount::new(
                otter_runtime::ResourceLimits::builder()
                    .limit(ResourceClass::ExternalBytes, server_bytes)
                    .build(),
            ),
            max_body_bytes,
        }
    }

    fn reserve(&self, amount: u64) -> Result<ServeBodyCharge, ServeBodyError> {
        self.preflight(amount)?;
        let server = self
            .server
            .reserve_exact(ResourceClass::ExternalBytes, amount)
            .map_err(ServeBodyError::ServerBudget)?;
        let runtime = self
            .runtime
            .reserve_exact(ResourceClass::ExternalBytes, amount)
            .map_err(ServeBodyError::RuntimeBudget)?;
        Ok(ServeBodyCharge { server, runtime })
    }

    /// Reject a known body length before polling or allocating its payload.
    pub(crate) fn preflight(&self, amount: u64) -> Result<(), ServeBodyError> {
        if amount > self.max_body_bytes {
            return Err(ServeBodyError::TooLarge {
                requested: amount,
                limit: self.max_body_bytes,
            });
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        runtime: ResourceAccount,
        max_body_bytes: u64,
        server_bytes: u64,
    ) -> Self {
        Self::with_limits(runtime, max_body_bytes, server_bytes)
    }
}

impl std::fmt::Debug for ServeBodyBudget {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServeBodyBudget")
            .field("max_body_bytes", &self.max_body_bytes)
            .finish_non_exhaustive()
    }
}

/// The paired charge owned for one native body allocation.
pub(crate) struct ServeBodyCharge {
    server: ResourceLease,
    runtime: ResourceLease,
}

impl ServeBodyCharge {
    fn resize(&mut self, amount: u64) -> Result<(), ServeBodyError> {
        let previous = self.server.amount();
        self.server
            .resize(amount)
            .map_err(ServeBodyError::ServerBudget)?;
        if let Err(error) = self.runtime.resize(amount) {
            self.server
                .resize(previous)
                .expect("rolling a server body charge back cannot fail");
            return Err(ServeBodyError::RuntimeBudget(error));
        }
        Ok(())
    }
}

impl std::fmt::Debug for ServeBodyCharge {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServeBodyCharge")
            .field("bytes", &self.runtime.amount())
            .finish_non_exhaustive()
    }
}

/// Failure to retain a native HTTP body.
#[derive(Debug, thiserror::Error)]
pub(crate) enum ServeBodyError {
    #[error("body of {requested} bytes exceeds the {limit}-byte buffered-body limit")]
    TooLarge { requested: u64, limit: u64 },
    #[error("server buffered-body limit: {0}")]
    ServerBudget(ResourceError),
    #[error("runtime external-memory budget: {0}")]
    RuntimeBudget(ResourceError),
    #[error("failed to allocate buffered body: {0}")]
    Allocation(TryReserveError),
    #[error("body byte length overflow")]
    LengthOverflow,
    #[error("response body buffer became detached or out of bounds")]
    Unavailable,
    #[error("response body streams are not supported yet; return a buffered Response body")]
    Unsupported,
}

/// Incrementally accumulated, accounted request body.
#[must_use = "dropping the builder releases its partial body charges"]
pub(crate) struct ServeBodyBuilder {
    bytes: Vec<u8>,
    charge: ServeBodyCharge,
    max_body_bytes: u64,
}

impl ServeBodyBuilder {
    pub(crate) fn new(budget: &ServeBodyBudget) -> Self {
        let charge = budget
            .reserve(0)
            .expect("a zero-byte body fits every valid serve budget");
        Self {
            bytes: Vec::new(),
            charge,
            max_body_bytes: budget.max_body_bytes,
        }
    }

    /// Admit and append one incoming HTTP data frame.
    pub(crate) fn push_bytes(&mut self, chunk: &[u8]) -> Result<(), ServeBodyError> {
        if chunk.is_empty() {
            return Ok(());
        }
        let new_len = self
            .bytes
            .len()
            .checked_add(chunk.len())
            .ok_or(ServeBodyError::LengthOverflow)?;
        let new_amount = body_amount(new_len)?;
        if new_amount > self.max_body_bytes {
            return Err(ServeBodyError::TooLarge {
                requested: new_amount,
                limit: self.max_body_bytes,
            });
        }

        let previous = body_amount(self.bytes.len())?;
        self.charge.resize(new_amount)?;
        if let Err(error) = self.bytes.try_reserve_exact(chunk.len()) {
            self.charge
                .resize(previous)
                .expect("rolling an allocation admission back cannot fail");
            return Err(ServeBodyError::Allocation(error));
        }
        self.bytes.extend_from_slice(chunk);
        Ok(())
    }

    #[must_use]
    pub(crate) fn finish(self) -> ServeBody {
        if self.bytes.is_empty() {
            ServeBody::Empty
        } else {
            ServeBody::Buffered {
                bytes: self.bytes,
                charge: self.charge,
            }
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.bytes.len()
    }
}

/// Owned HTTP body payload for server request/response dispatch.
#[derive(Debug, Default)]
pub(crate) enum ServeBody {
    #[default]
    Empty,
    Buffered {
        bytes: Vec<u8>,
        charge: ServeBodyCharge,
    },
}

impl ServeBody {
    fn allocate(
        budget: &ServeBodyBudget,
        len: usize,
    ) -> Result<(Vec<u8>, ServeBodyCharge), ServeBodyError> {
        let charge = budget.reserve(body_amount(len)?)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(len)
            .map_err(ServeBodyError::Allocation)?;
        Ok((bytes, charge))
    }

    #[cfg(test)]
    pub(crate) fn copy_from_slice(
        budget: &ServeBodyBudget,
        source: &[u8],
    ) -> Result<Self, ServeBodyError> {
        let mut builder = ServeBodyBuilder::new(budget);
        builder.push_bytes(source)?;
        Ok(builder.finish())
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) fn as_buffered_bytes(&self) -> &[u8] {
        match self {
            Self::Empty => &[],
            Self::Buffered { bytes, .. } => bytes,
        }
    }

    /// Move bytes and their charge into the HTTP response body.
    #[must_use]
    pub(crate) fn into_buffered_parts(self) -> (Vec<u8>, Option<ServeBodyCharge>) {
        match self {
            Self::Empty => (Vec::new(), None),
            Self::Buffered { bytes, charge } => (bytes, Some(charge)),
        }
    }

    /// Convert request bytes into the Fetch internals body slot.
    pub(crate) fn to_js_body(&self, ctx: &mut NativeCtx<'_>) -> Result<Value, NativeError> {
        match self {
            Self::Empty => Ok(Value::null()),
            Self::Buffered { bytes, .. } => {
                bytes_to_uint8_array(ctx, bytes.clone(), "serve.request")
            }
        }
    }

    /// Copy a buffered response body out of Fetch internals under both budgets.
    pub(crate) fn from_js_value(
        ctx: &mut NativeCtx<'_>,
        value: Value,
        budget: &ServeBodyBudget,
    ) -> Result<Self, NativeError> {
        Self::from_js_value_inner(ctx, value, budget)
            .map_err(|error| runtime_type_error("serve.response", error.to_string()))
    }

    fn from_js_value_inner(
        ctx: &mut NativeCtx<'_>,
        value: Value,
        budget: &ServeBodyBudget,
    ) -> Result<Self, ServeBodyError> {
        if value.is_null() || value.is_undefined() {
            return Ok(Self::Empty);
        }
        if let Some(string) = value.as_string(ctx.heap()) {
            return copy_js_string(ctx, string, budget);
        }
        if let Some(typed_array) = value.as_typed_array(ctx.heap()) {
            let offset = typed_array.byte_offset(ctx.heap());
            let len = typed_array.byte_length(ctx.heap());
            let end = offset
                .checked_add(len)
                .ok_or(ServeBodyError::LengthOverflow)?;
            let buffer = typed_array.buffer(ctx.heap());
            let (mut bytes, charge) = Self::allocate(budget, len)?;
            let copied = buffer.with_bytes(ctx.heap(), |source| {
                let Some(source) = source.get(offset..end) else {
                    return false;
                };
                bytes.extend_from_slice(source);
                true
            });
            if !copied {
                return Err(ServeBodyError::Unavailable);
            }
            return Ok(if bytes.is_empty() {
                Self::Empty
            } else {
                Self::Buffered { bytes, charge }
            });
        }
        if let Some(buffer) = value.as_array_buffer() {
            let len = buffer.byte_length(ctx.heap());
            let (mut bytes, charge) = Self::allocate(budget, len)?;
            let copied = buffer.with_bytes(ctx.heap(), |source| {
                let Some(source) = source.get(..len) else {
                    return false;
                };
                bytes.extend_from_slice(source);
                true
            });
            if !copied {
                return Err(ServeBodyError::Unavailable);
            }
            return Ok(if bytes.is_empty() {
                Self::Empty
            } else {
                Self::Buffered { bytes, charge }
            });
        }
        Err(ServeBodyError::Unsupported)
    }
}

fn copy_js_string(
    ctx: &NativeCtx<'_>,
    string: JsString,
    budget: &ServeBodyBudget,
) -> Result<ServeBody, ServeBodyError> {
    let len = js_string_utf8_len(ctx, string)?;
    let (mut bytes, charge) = ServeBody::allocate(budget, len)?;

    if string
        .with_latin1(ctx.heap(), |source| {
            for byte in source {
                push_char(&mut bytes, char::from(*byte));
            }
        })
        .is_none()
    {
        string.with_utf16(ctx.heap(), |source| {
            for decoded in char::decode_utf16(source.iter().copied()) {
                push_char(&mut bytes, decoded.unwrap_or(char::REPLACEMENT_CHARACTER));
            }
        });
    }
    debug_assert_eq!(bytes.len(), len);
    Ok(if bytes.is_empty() {
        ServeBody::Empty
    } else {
        ServeBody::Buffered { bytes, charge }
    })
}

fn js_string_utf8_len(ctx: &NativeCtx<'_>, string: JsString) -> Result<usize, ServeBodyError> {
    if let Some(len) = string.with_latin1(ctx.heap(), |source| {
        source.iter().try_fold(0_usize, |len, byte| {
            len.checked_add(char::from(*byte).len_utf8())
        })
    }) {
        return len.ok_or(ServeBodyError::LengthOverflow);
    }
    string.with_utf16(ctx.heap(), |source| {
        char::decode_utf16(source.iter().copied())
            .map(|decoded| decoded.unwrap_or(char::REPLACEMENT_CHARACTER).len_utf8())
            .try_fold(0_usize, usize::checked_add)
            .ok_or(ServeBodyError::LengthOverflow)
    })
}

fn push_char(bytes: &mut Vec<u8>, character: char) {
    let mut encoded = [0_u8; 4];
    bytes.extend_from_slice(character.encode_utf8(&mut encoded).as_bytes());
}

fn body_amount(len: usize) -> Result<u64, ServeBodyError> {
    u64::try_from(len).map_err(|_| ServeBodyError::LengthOverflow)
}

fn bytes_to_uint8_array(
    ctx: &mut NativeCtx<'_>,
    bytes: Vec<u8>,
    name: &'static str,
) -> Result<Value, NativeError> {
    let buffer = ctx
        .array_buffer_from_bytes(bytes)
        .map_err(|err| runtime_type_error(name, err.to_string()))?;
    let ctor = ctx
        .global_value("Uint8Array")
        .ok_or_else(|| runtime_type_error(name, "Uint8Array is unavailable"))?;
    ctx.construct(ctor, &[Value::array_buffer(buffer)])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn external_bytes(account: &ResourceAccount) -> u64 {
        account
            .snapshot()
            .get(ResourceClass::ExternalBytes)
            .current()
    }

    #[test]
    fn incremental_body_charges_and_releases_exact_bytes() {
        let runtime = ResourceAccount::default();
        let budget = ServeBodyBudget::for_test(runtime.clone(), 16, 32);
        let mut builder = ServeBodyBuilder::new(&budget);

        builder.push_bytes(b"abc").unwrap();
        builder.push_bytes(b"defg").unwrap();
        assert_eq!(builder.len(), 7);
        assert_eq!(external_bytes(&runtime), 7);

        let body = builder.finish();
        assert_eq!(body.as_buffered_bytes(), b"abcdefg");
        assert_eq!(external_bytes(&runtime), 7);
        drop(body);
        assert_eq!(external_bytes(&runtime), 0);
    }

    #[test]
    fn body_limit_rejects_without_growing_or_leaking_the_charge() {
        let runtime = ResourceAccount::default();
        let budget = ServeBodyBudget::for_test(runtime.clone(), 4, 32);
        let mut builder = ServeBodyBuilder::new(&budget);
        builder.push_bytes(b"abc").unwrap();

        let error = builder.push_bytes(b"de").unwrap_err();
        assert!(matches!(
            error,
            ServeBodyError::TooLarge {
                requested: 5,
                limit: 4
            }
        ));
        assert_eq!(builder.len(), 3);
        assert_eq!(external_bytes(&runtime), 3);
        drop(builder);
        assert_eq!(external_bytes(&runtime), 0);
    }

    #[test]
    fn finite_server_budget_still_applies_with_an_unlimited_runtime() {
        let runtime = ResourceAccount::default();
        let budget = ServeBodyBudget::for_test(runtime.clone(), 8, 5);
        let first = ServeBody::copy_from_slice(&budget, b"abcd").unwrap();

        let error = ServeBody::copy_from_slice(&budget, b"ef").unwrap_err();
        assert!(matches!(error, ServeBodyError::ServerBudget(_)));
        assert_eq!(external_bytes(&runtime), 4);
        drop(first);
        assert_eq!(external_bytes(&runtime), 0);
    }

    #[test]
    fn runtime_rejection_preserves_partial_body_and_recovers() {
        let runtime = ResourceAccount::new(
            otter_runtime::ResourceLimits::builder()
                .limit(ResourceClass::ExternalBytes, 4)
                .build(),
        );
        let budget = ServeBodyBudget::for_test(runtime.clone(), 8, 16);
        let mut builder = ServeBodyBuilder::new(&budget);
        builder.push_bytes(b"abc").unwrap();

        let error = builder.push_bytes(b"de").unwrap_err();
        assert!(matches!(error, ServeBodyError::RuntimeBudget(_)));
        assert_eq!(builder.len(), 3);
        assert_eq!(external_bytes(&runtime), 3);
        drop(builder);
        assert_eq!(external_bytes(&runtime), 0);
    }
}
