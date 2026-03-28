//! Native payload storage and tracing contracts for JS-visible host objects.

use core::any::{Any, type_name};

use crate::object::{ObjectError, ObjectHandle};
use crate::value::RegisterValue;

/// Stable identifier of one native payload stored in the runtime registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct NativePayloadId(pub u32);

/// Error produced while resolving or tracing native payloads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativePayloadError {
    /// The runtime value was expected to be an object handle.
    ExpectedObjectValue,
    /// The referenced object handle does not exist in the current heap.
    InvalidObjectHandle,
    /// The object exists, but does not carry a native payload link.
    MissingPayload,
    /// The payload link points outside the current payload registry.
    InvalidPayloadId,
    /// The payload exists, but does not match the requested Rust type.
    TypeMismatch { expected: &'static str },
}

impl core::fmt::Display for NativePayloadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ExpectedObjectValue => f.write_str("value must be an object handle"),
            Self::InvalidObjectHandle => f.write_str("object handle is outside the current heap"),
            Self::MissingPayload => f.write_str("object does not carry a native payload"),
            Self::InvalidPayloadId => f.write_str("native payload id is outside the registry"),
            Self::TypeMismatch { expected } => {
                write!(f, "native payload does not match expected type {expected}")
            }
        }
    }
}

impl std::error::Error for NativePayloadError {}

impl From<ObjectError> for NativePayloadError {
    fn from(value: ObjectError) -> Self {
        match value {
            ObjectError::InvalidHandle => Self::InvalidObjectHandle,
            ObjectError::InvalidKind
            | ObjectError::InvalidIndex
            | ObjectError::InvalidArrayLength => Self::MissingPayload,
        }
    }
}

/// Tracing sink for GC-visible values stored inside native payloads.
pub trait VmValueTracer {
    /// Records one GC-visible register value.
    fn mark_value(&mut self, value: RegisterValue);

    /// Records one GC-visible object handle.
    fn mark_object(&mut self, handle: ObjectHandle) {
        self.mark_value(RegisterValue::from_object_handle(handle.0));
    }
}

/// Tracing contract for Rust-authored payload types that hold VM references.
///
/// `#[js_class]`-style host classes must implement this trait for any payload
/// that survives calls, exceptions, or future suspension points. Any
/// `RegisterValue`/`ObjectHandle` stored in the payload must be reported here.
pub trait VmTrace {
    /// Reports GC-visible references held by this payload.
    fn trace(&self, tracer: &mut dyn VmValueTracer);
}

impl VmTrace for () {
    fn trace(&self, _tracer: &mut dyn VmValueTracer) {}
}

impl VmTrace for RegisterValue {
    fn trace(&self, tracer: &mut dyn VmValueTracer) {
        if self.as_object_handle().is_some() {
            tracer.mark_value(*self);
        }
    }
}

impl VmTrace for ObjectHandle {
    fn trace(&self, tracer: &mut dyn VmValueTracer) {
        tracer.mark_object(*self);
    }
}

impl<T: VmTrace> VmTrace for Option<T> {
    fn trace(&self, tracer: &mut dyn VmValueTracer) {
        if let Some(value) = self {
            value.trace(tracer);
        }
    }
}

impl<T: VmTrace> VmTrace for Vec<T> {
    fn trace(&self, tracer: &mut dyn VmValueTracer) {
        for value in self {
            value.trace(tracer);
        }
    }
}

impl<T: VmTrace> VmTrace for Box<T> {
    fn trace(&self, tracer: &mut dyn VmValueTracer) {
        self.as_ref().trace(tracer);
    }
}

/// Type-erased runtime payload owned by the VM.
pub trait VmNativePayload: VmTrace {
    /// Returns an untyped shared reference for downcasting.
    fn as_any(&self) -> &dyn Any;

    /// Returns an untyped mutable reference for downcasting.
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

impl<T> VmNativePayload for T
where
    T: VmTrace + Any,
{
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

#[derive(Default)]
struct NativePayloadSlot {
    payload: Option<Box<dyn VmNativePayload>>,
}

/// Runtime-owned registry for erased native instance payloads.
#[derive(Default)]
pub struct NativePayloadRegistry {
    slots: Vec<NativePayloadSlot>,
}

impl NativePayloadRegistry {
    /// Creates an empty payload registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores one payload and returns its stable id.
    pub fn insert<T>(&mut self, payload: T) -> NativePayloadId
    where
        T: VmTrace + Any,
    {
        let id = NativePayloadId(u32::try_from(self.slots.len()).unwrap_or(u32::MAX));
        self.slots.push(NativePayloadSlot {
            payload: Some(Box::new(payload)),
        });
        id
    }

    /// Borrows one payload by id and downcasts it to the requested Rust type.
    pub fn get<T>(&self, id: NativePayloadId) -> Result<&T, NativePayloadError>
    where
        T: Any,
    {
        let payload = self.payload(id)?;
        payload
            .as_any()
            .downcast_ref::<T>()
            .ok_or(NativePayloadError::TypeMismatch {
                expected: type_name::<T>(),
            })
    }

    /// Mutably borrows one payload by id and downcasts it to the requested Rust type.
    pub fn get_mut<T>(&mut self, id: NativePayloadId) -> Result<&mut T, NativePayloadError>
    where
        T: Any,
    {
        let payload = self.payload_mut(id)?;
        payload
            .as_any_mut()
            .downcast_mut::<T>()
            .ok_or(NativePayloadError::TypeMismatch {
                expected: type_name::<T>(),
            })
    }

    /// Traces one payload by id through the provided GC root sink.
    pub fn trace_payload(
        &self,
        id: NativePayloadId,
        tracer: &mut dyn VmValueTracer,
    ) -> Result<(), NativePayloadError> {
        self.payload(id)?.trace(tracer);
        Ok(())
    }

    fn payload(
        &self,
        id: NativePayloadId,
    ) -> Result<&(dyn VmNativePayload + '_), NativePayloadError> {
        let Some(slot) = self.slots.get(usize::try_from(id.0).unwrap_or(usize::MAX)) else {
            return Err(NativePayloadError::InvalidPayloadId);
        };
        let Some(payload) = slot.payload.as_deref() else {
            return Err(NativePayloadError::InvalidPayloadId);
        };
        Ok(payload)
    }

    fn payload_mut(
        &mut self,
        id: NativePayloadId,
    ) -> Result<&mut (dyn VmNativePayload + '_), NativePayloadError> {
        let Some(slot) = self
            .slots
            .get_mut(usize::try_from(id.0).unwrap_or(usize::MAX))
        else {
            return Err(NativePayloadError::InvalidPayloadId);
        };
        let Some(payload) = slot.payload.as_deref_mut() else {
            return Err(NativePayloadError::InvalidPayloadId);
        };
        Ok(payload)
    }
}

#[cfg(test)]
mod tests {
    use super::{NativePayloadError, NativePayloadRegistry, VmTrace, VmValueTracer};
    use crate::object::ObjectHandle;
    use crate::value::RegisterValue;

    #[derive(Debug, PartialEq)]
    struct Payload {
        root: RegisterValue,
        nested: Option<ObjectHandle>,
    }

    impl VmTrace for Payload {
        fn trace(&self, tracer: &mut dyn VmValueTracer) {
            self.root.trace(tracer);
            self.nested.trace(tracer);
        }
    }

    #[derive(Default)]
    struct CollectingTracer {
        values: Vec<RegisterValue>,
    }

    impl VmValueTracer for CollectingTracer {
        fn mark_value(&mut self, value: RegisterValue) {
            self.values.push(value);
        }
    }

    #[test]
    fn native_payload_registry_downcasts_and_traces() {
        let mut registry = NativePayloadRegistry::new();
        let object = ObjectHandle(7);
        let payload_id = registry.insert(Payload {
            root: RegisterValue::from_object_handle(object.0),
            nested: Some(ObjectHandle(9)),
        });

        let payload = registry
            .get::<Payload>(payload_id)
            .expect("payload should downcast");
        assert_eq!(payload.root, RegisterValue::from_object_handle(7));

        let mut tracer = CollectingTracer::default();
        registry
            .trace_payload(payload_id, &mut tracer)
            .expect("payload trace should succeed");
        assert_eq!(
            tracer.values,
            vec![
                RegisterValue::from_object_handle(7),
                RegisterValue::from_object_handle(9),
            ]
        );
    }

    #[test]
    fn native_payload_registry_reports_type_mismatch() {
        let mut registry = NativePayloadRegistry::new();
        let payload_id = registry.insert(Payload {
            root: RegisterValue::undefined(),
            nested: None,
        });

        let error = registry
            .get::<Vec<RegisterValue>>(payload_id)
            .expect_err("wrong payload type should fail");
        assert_eq!(
            error,
            NativePayloadError::TypeMismatch {
                expected: core::any::type_name::<Vec<RegisterValue>>(),
            }
        );
    }
}
