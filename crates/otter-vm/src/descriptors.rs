//! Descriptor layer between proc-macros and runtime builders.
//!
//! Proc-macros should expand into descriptors defined here instead of mutating
//! runtime/bootstrap state directly. Builders and intrinsic installers consume
//! these descriptors and perform the actual object/property installation.

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
///
/// New-VM descriptors are pure static metadata, so the callback shape stays a
/// plain function pointer instead of a heap-allocated shared closure.
pub type VmNativeFunction = fn(
    &RegisterValue,
    &[RegisterValue],
    &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError>;

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
    /// §10.1.14 — Default `intrinsicDefaultProto` to use when this descriptor is
    /// invoked as a constructor and `newTarget.prototype` is not an object.
    /// `None` for non-constructor descriptors and constructors that have no
    /// realm-scoped intrinsic prototype (use Object.prototype).
    default_intrinsic: Option<crate::intrinsics::IntrinsicKey>,
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
            default_intrinsic: None,
        }
    }

    /// Sets the default `intrinsicDefaultProto` (§10.1.14) used when this
    /// descriptor is invoked as a constructor and `newTarget.prototype` is not
    /// an object.
    #[must_use]
    pub fn with_default_intrinsic(mut self, key: crate::intrinsics::IntrinsicKey) -> Self {
        self.default_intrinsic = Some(key);
        self
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

    /// Returns the `intrinsicDefaultProto` (§10.1.14) for this descriptor when
    /// it acts as a constructor, if any.
    #[must_use]
    pub const fn default_intrinsic(&self) -> Option<crate::intrinsics::IntrinsicKey> {
        self.default_intrinsic
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

/// Pure metadata for one JS-visible namespace in the new VM.
///
/// Unlike classes, namespaces install onto one object target only and do not
/// carry constructor/prototype semantics.
#[derive(Clone, Default)]
pub struct JsNamespaceDescriptor {
    js_name: Box<str>,
    bindings: Vec<NativeBindingDescriptor>,
}

impl JsNamespaceDescriptor {
    /// Creates an empty namespace descriptor for the given JS-visible name.
    #[must_use]
    pub fn new(js_name: impl Into<Box<str>>) -> Self {
        Self {
            js_name: js_name.into(),
            bindings: Vec::new(),
        }
    }

    /// Adds one namespace binding.
    #[must_use]
    pub fn with_binding(mut self, binding: NativeBindingDescriptor) -> Self {
        self.bindings.push(binding);
        self
    }

    /// Returns the JS-visible namespace name.
    #[must_use]
    pub fn js_name(&self) -> &str {
        &self.js_name
    }

    /// Returns the namespace bindings that should install onto the namespace object.
    #[must_use]
    pub fn bindings(&self) -> &[NativeBindingDescriptor] {
        &self.bindings
    }
}

/// Contract that future namespace builders should implement to consume macro-generated metadata.
pub trait NamespaceDescriptorConsumer {
    /// Registers one namespace descriptor for later installation.
    fn register_namespace(&mut self, descriptor: JsNamespaceDescriptor);
}

/// Pure metadata for one JS-visible class in the new VM.
///
/// This intentionally separates class metadata from runtime installation. A
/// builder can consume the descriptor, install the constructor callback (if
/// present), then map each binding to prototype-vs-constructor property
/// installation based on [`NativeBindingTarget`].
#[derive(Clone, Default)]
pub struct JsClassDescriptor {
    js_name: Box<str>,
    constructor: Option<NativeFunctionDescriptor>,
    bindings: Vec<NativeBindingDescriptor>,
}

impl JsClassDescriptor {
    /// Creates an empty class descriptor for the given JS-visible class name.
    #[must_use]
    pub fn new(js_name: impl Into<Box<str>>) -> Self {
        Self {
            js_name: js_name.into(),
            constructor: None,
            bindings: Vec::new(),
        }
    }

    /// Attaches constructor metadata for the class.
    #[must_use]
    pub fn with_constructor(mut self, constructor: NativeFunctionDescriptor) -> Self {
        self.constructor = Some(constructor);
        self
    }

    /// Adds one prototype/static binding to the class descriptor.
    #[must_use]
    pub fn with_binding(mut self, binding: NativeBindingDescriptor) -> Self {
        self.bindings.push(binding);
        self
    }

    /// Returns the JS-visible class name.
    #[must_use]
    pub fn js_name(&self) -> &str {
        &self.js_name
    }

    /// Returns the constructor metadata if the class exposes one.
    #[must_use]
    pub const fn constructor(&self) -> Option<&NativeFunctionDescriptor> {
        self.constructor.as_ref()
    }

    /// Returns the class bindings that should be installed on the prototype or constructor object.
    #[must_use]
    pub fn bindings(&self) -> &[NativeBindingDescriptor] {
        &self.bindings
    }
}

/// Contract that future class builders should implement to consume macro-generated class descriptors.
pub trait ClassDescriptorConsumer {
    /// Registers one class descriptor for later installation.
    fn register_class(&mut self, descriptor: JsClassDescriptor);
}

#[cfg(test)]
mod tests {
    use super::{
        ClassDescriptorConsumer, JsClassDescriptor, JsNamespaceDescriptor,
        NamespaceDescriptorConsumer, NativeBindingDescriptor, NativeBindingTarget,
        NativeDescriptorConsumer, NativeEntrypointKind, NativeFunctionDescriptor, NativeSlotKind,
        VmNativeCallError, VmNativeFunction,
    };
    use crate::value::RegisterValue;

    fn passthrough_callback() -> VmNativeFunction {
        passthrough
    }

    fn passthrough(
        this: &RegisterValue,
        _args: &[RegisterValue],
        _runtime: &mut crate::interpreter::RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError> {
        Ok(*this)
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

    #[test]
    fn js_namespace_descriptor_keeps_bindings() {
        let descriptor =
            JsNamespaceDescriptor::new("Reflect").with_binding(NativeBindingDescriptor::new(
                NativeBindingTarget::Namespace,
                NativeFunctionDescriptor::method("get", 2, passthrough_callback()),
            ));

        assert_eq!(descriptor.js_name(), "Reflect");
        assert_eq!(descriptor.bindings().len(), 1);
        assert_eq!(
            descriptor.bindings()[0].target(),
            NativeBindingTarget::Namespace
        );
        assert_eq!(descriptor.bindings()[0].function().js_name(), "get");
    }

    #[test]
    fn js_class_descriptor_keeps_constructor_and_bindings() {
        let descriptor = JsClassDescriptor::new("Thing")
            .with_constructor(NativeFunctionDescriptor::constructor(
                "Thing",
                1,
                passthrough_callback(),
            ))
            .with_binding(NativeBindingDescriptor::new(
                NativeBindingTarget::Prototype,
                NativeFunctionDescriptor::method("valueOf", 0, passthrough_callback()),
            ))
            .with_binding(NativeBindingDescriptor::new(
                NativeBindingTarget::Constructor,
                NativeFunctionDescriptor::getter("version", passthrough_callback()),
            ));

        assert_eq!(descriptor.js_name(), "Thing");
        assert_eq!(
            descriptor
                .constructor()
                .map(NativeFunctionDescriptor::js_name),
            Some("Thing")
        );
        assert_eq!(descriptor.bindings().len(), 2);
        assert_eq!(
            descriptor.bindings()[0].target(),
            NativeBindingTarget::Prototype
        );
        assert_eq!(
            descriptor.bindings()[1].function().slot_kind(),
            NativeSlotKind::Getter
        );
    }

    #[test]
    fn class_descriptor_consumer_receives_class_metadata() {
        #[derive(Default)]
        struct TestConsumer {
            seen: Vec<JsClassDescriptor>,
        }

        impl ClassDescriptorConsumer for TestConsumer {
            fn register_class(&mut self, descriptor: JsClassDescriptor) {
                self.seen.push(descriptor);
            }
        }

        let mut consumer = TestConsumer::default();
        consumer.register_class(JsClassDescriptor::new("Counter").with_binding(
            NativeBindingDescriptor::new(
                NativeBindingTarget::Prototype,
                NativeFunctionDescriptor::method("inc", 0, passthrough_callback()),
            ),
        ));

        assert_eq!(consumer.seen.len(), 1);
        assert_eq!(consumer.seen[0].js_name(), "Counter");
        assert_eq!(consumer.seen[0].bindings()[0].function().js_name(), "inc");
    }

    #[test]
    fn namespace_descriptor_consumer_receives_namespace_metadata() {
        #[derive(Default)]
        struct TestConsumer {
            seen: Vec<JsNamespaceDescriptor>,
        }

        impl NamespaceDescriptorConsumer for TestConsumer {
            fn register_namespace(&mut self, descriptor: JsNamespaceDescriptor) {
                self.seen.push(descriptor);
            }
        }

        let mut consumer = TestConsumer::default();
        consumer.register_namespace(JsNamespaceDescriptor::new("JSON").with_binding(
            NativeBindingDescriptor::new(
                NativeBindingTarget::Namespace,
                NativeFunctionDescriptor::method("parse", 1, passthrough_callback()),
            ),
        ));

        assert_eq!(consumer.seen.len(), 1);
        assert_eq!(consumer.seen[0].js_name(), "JSON");
        assert_eq!(consumer.seen[0].bindings()[0].function().js_name(), "parse");
    }
}
