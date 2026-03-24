//! Descriptor layer between proc-macros and runtime builders.
//!
//! Proc-macros should expand into descriptors defined here instead of mutating
//! runtime/bootstrap state directly. Builders and intrinsic installers consume
//! these descriptors and perform the actual object/property installation.

use std::sync::Arc;

use crate::interpreter::RuntimeState;
use crate::value::RegisterValue;

/// Error produced by a native host function exposed through the new VM.
#[derive(Debug, Clone, PartialEq)]
pub enum VmNativeCallError {
    /// The native entrypoint raised a JS-visible thrown value.
    Thrown(RegisterValue),
    /// The native entrypoint failed before it could produce a JS-visible result.
    Internal(Box<str>),
}

impl core::fmt::Display for VmNativeCallError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Thrown(value) => write!(f, "native function threw {:?}", value),
            Self::Internal(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for VmNativeCallError {}

/// Runtime ABI of a native host function exposed through the new VM.
pub type VmNativeFunction = Arc<
    dyn Fn(
            &RegisterValue,
            &[RegisterValue],
            &mut RuntimeState,
        ) -> Result<RegisterValue, VmNativeCallError>
        + Send
        + Sync,
>;

/// Whether a native entrypoint executes synchronously or represents an async-capable hook.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NativeEntrypointKind {
    Sync,
    Async,
}

/// Property semantics that a builder should apply to a native descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NativeSlotKind {
    Method,
    Getter,
    Setter,
    Constructor,
}

/// Installation target for a native descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NativeBindingTarget {
    Prototype,
    Constructor,
    Namespace,
    Global,
}

/// Pure metadata for one JS-callable native function.
///
/// This is the output shape that future `#[dive]`-style proc-macros should
/// target. It does not install itself anywhere.
#[derive(Clone)]
pub struct NativeFunctionDescriptor {
    js_name: Box<str>,
    length: u16,
    entrypoint_kind: NativeEntrypointKind,
    slot_kind: NativeSlotKind,
    callback: VmNativeFunction,
}

impl NativeFunctionDescriptor {
    /// Creates a new descriptor with an explicit slot kind.
    #[must_use]
    pub fn new(
        js_name: impl Into<Box<str>>,
        length: u16,
        entrypoint_kind: NativeEntrypointKind,
        slot_kind: NativeSlotKind,
        callback: VmNativeFunction,
    ) -> Self {
        Self {
            js_name: js_name.into(),
            length,
            entrypoint_kind,
            slot_kind,
            callback,
        }
    }

    /// Convenience constructor for an instance or namespace method.
    #[must_use]
    pub fn method(js_name: impl Into<Box<str>>, length: u16, callback: VmNativeFunction) -> Self {
        Self::new(
            js_name,
            length,
            NativeEntrypointKind::Sync,
            NativeSlotKind::Method,
            callback,
        )
    }

    /// Convenience constructor for an async-capable method descriptor.
    #[must_use]
    pub fn async_method(
        js_name: impl Into<Box<str>>,
        length: u16,
        callback: VmNativeFunction,
    ) -> Self {
        Self::new(
            js_name,
            length,
            NativeEntrypointKind::Async,
            NativeSlotKind::Method,
            callback,
        )
    }

    /// Convenience constructor for a getter descriptor.
    #[must_use]
    pub fn getter(js_name: impl Into<Box<str>>, callback: VmNativeFunction) -> Self {
        Self::new(
            js_name,
            0,
            NativeEntrypointKind::Sync,
            NativeSlotKind::Getter,
            callback,
        )
    }

    /// Convenience constructor for a setter descriptor.
    #[must_use]
    pub fn setter(js_name: impl Into<Box<str>>, callback: VmNativeFunction) -> Self {
        Self::new(
            js_name,
            1,
            NativeEntrypointKind::Sync,
            NativeSlotKind::Setter,
            callback,
        )
    }

    /// Convenience constructor for a constructor descriptor.
    #[must_use]
    pub fn constructor(
        js_name: impl Into<Box<str>>,
        length: u16,
        callback: VmNativeFunction,
    ) -> Self {
        Self::new(
            js_name,
            length,
            NativeEntrypointKind::Sync,
            NativeSlotKind::Constructor,
            callback,
        )
    }

    /// Returns the JS-visible property name.
    #[must_use]
    pub fn js_name(&self) -> &str {
        &self.js_name
    }

    /// Returns the JS-visible `.length`.
    #[must_use]
    pub const fn length(&self) -> u16 {
        self.length
    }

    /// Returns whether the descriptor is sync or async.
    #[must_use]
    pub const fn entrypoint_kind(&self) -> NativeEntrypointKind {
        self.entrypoint_kind
    }

    /// Returns the property semantics of the descriptor.
    #[must_use]
    pub const fn slot_kind(&self) -> NativeSlotKind {
        self.slot_kind
    }

    /// Returns the callable entrypoint.
    #[must_use]
    pub fn callback(&self) -> &VmNativeFunction {
        &self.callback
    }
}

/// Builder-facing contract for installing one native descriptor.
///
/// Macros should emit this metadata. Builders decide how it becomes actual
/// properties on constructor/prototype/namespace/global objects.
#[derive(Clone)]
pub struct NativeBindingDescriptor {
    target: NativeBindingTarget,
    function: NativeFunctionDescriptor,
}

impl NativeBindingDescriptor {
    /// Creates a new binding descriptor.
    #[must_use]
    pub const fn new(target: NativeBindingTarget, function: NativeFunctionDescriptor) -> Self {
        Self { target, function }
    }

    /// Returns the builder installation target.
    #[must_use]
    pub const fn target(&self) -> NativeBindingTarget {
        self.target
    }

    /// Returns the function descriptor carried by this binding.
    #[must_use]
    pub const fn function(&self) -> &NativeFunctionDescriptor {
        &self.function
    }
}

/// Contract that future builders should implement to consume macro-generated native descriptors.
pub trait NativeDescriptorConsumer {
    /// Registers one native binding descriptor for later installation.
    fn register_native(&mut self, descriptor: NativeBindingDescriptor);
}

#[cfg(test)]
mod tests {
    use super::{
        NativeBindingDescriptor, NativeBindingTarget, NativeDescriptorConsumer,
        NativeEntrypointKind, NativeFunctionDescriptor, NativeSlotKind, VmNativeCallError,
        VmNativeFunction,
    };
    use crate::value::RegisterValue;

    fn passthrough_callback() -> VmNativeFunction {
        std::sync::Arc::new(|this, _args, _runtime| Ok(*this))
    }

    #[test]
    fn native_function_descriptor_keeps_metadata() {
        let descriptor =
            NativeFunctionDescriptor::async_method("fetchThing", 2, passthrough_callback());

        assert_eq!(descriptor.js_name(), "fetchThing");
        assert_eq!(descriptor.length(), 2);
        assert_eq!(descriptor.entrypoint_kind(), NativeEntrypointKind::Async);
        assert_eq!(descriptor.slot_kind(), NativeSlotKind::Method);

        let value =
            (descriptor.callback())(&RegisterValue::from_i32(7), &[], &mut Default::default())
                .expect("callback should succeed");
        assert_eq!(value, RegisterValue::from_i32(7));
    }

    #[test]
    fn native_binding_descriptor_carries_target_contract() {
        let function = NativeFunctionDescriptor::getter("signal", passthrough_callback());
        let binding = NativeBindingDescriptor::new(NativeBindingTarget::Prototype, function);

        assert_eq!(binding.target(), NativeBindingTarget::Prototype);
        assert_eq!(binding.function().js_name(), "signal");
        assert_eq!(binding.function().slot_kind(), NativeSlotKind::Getter);
    }

    #[test]
    fn native_descriptor_consumer_receives_macro_style_metadata() {
        #[derive(Default)]
        struct TestConsumer {
            seen: Vec<NativeBindingDescriptor>,
        }

        impl NativeDescriptorConsumer for TestConsumer {
            fn register_native(&mut self, descriptor: NativeBindingDescriptor) {
                self.seen.push(descriptor);
            }
        }

        let mut consumer = TestConsumer::default();
        consumer.register_native(NativeBindingDescriptor::new(
            NativeBindingTarget::Namespace,
            NativeFunctionDescriptor::method("abs", 1, passthrough_callback()),
        ));

        assert_eq!(consumer.seen.len(), 1);
        assert_eq!(consumer.seen[0].target(), NativeBindingTarget::Namespace);
        assert_eq!(consumer.seen[0].function().js_name(), "abs");
    }

    #[test]
    fn native_call_error_formats_thrown_and_internal_paths() {
        let thrown = VmNativeCallError::Thrown(RegisterValue::from_i32(9));
        let internal = VmNativeCallError::Internal("native setup failed".into());

        assert!(thrown.to_string().contains("threw"));
        assert_eq!(internal.to_string(), "native setup failed");
    }
}
