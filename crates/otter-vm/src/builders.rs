//! Builder-side adapters for descriptor-driven bootstrap in the new VM.
//!
//! These builders intentionally stop at normalized installation plans. They do
//! not mutate runtime state directly; instead they validate and reshape macro
//! metadata so the eventual bootstrap layer has one explicit path to consume.

use crate::descriptors::{
    JsClassDescriptor, NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor,
    NativeSlotKind,
};

/// Error produced while normalizing class metadata into a builder install plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClassBuilderError {
    /// Class bindings may only target the prototype or constructor objects.
    InvalidBindingTarget {
        target: NativeBindingTarget,
        js_name: Box<str>,
    },
    /// The dedicated constructor slot must carry constructor metadata.
    InvalidConstructorDescriptor { slot_kind: NativeSlotKind },
    /// Constructor descriptors must not appear as ordinary bindings.
    ConstructorBindingNotAllowed {
        target: NativeBindingTarget,
        js_name: Box<str>,
    },
    /// Two members on the same target attempted to claim the same property.
    DuplicateMember {
        target: NativeBindingTarget,
        js_name: Box<str>,
    },
    /// A getter or setter conflicted with an already-declared method.
    ConflictingMemberKinds {
        target: NativeBindingTarget,
        js_name: Box<str>,
    },
    /// An accessor declared more than one getter on the same property.
    DuplicateGetter {
        target: NativeBindingTarget,
        js_name: Box<str>,
    },
    /// An accessor declared more than one setter on the same property.
    DuplicateSetter {
        target: NativeBindingTarget,
        js_name: Box<str>,
    },
}

impl core::fmt::Display for ClassBuilderError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidBindingTarget { target, js_name } => write!(
                f,
                "class binding '{js_name}' uses invalid target {target:?}; expected Prototype or Constructor"
            ),
            Self::InvalidConstructorDescriptor { slot_kind } => write!(
                f,
                "class constructor metadata must use slot kind Constructor, got {slot_kind:?}"
            ),
            Self::ConstructorBindingNotAllowed { target, js_name } => write!(
                f,
                "class binding '{js_name}' uses constructor slot metadata on target {target:?}"
            ),
            Self::DuplicateMember { target, js_name } => write!(
                f,
                "class target {target:?} already defines member '{js_name}'"
            ),
            Self::ConflictingMemberKinds { target, js_name } => write!(
                f,
                "class target {target:?} mixes method and accessor metadata for '{js_name}'"
            ),
            Self::DuplicateGetter { target, js_name } => write!(
                f,
                "class target {target:?} already defines a getter for '{js_name}'"
            ),
            Self::DuplicateSetter { target, js_name } => write!(
                f,
                "class target {target:?} already defines a setter for '{js_name}'"
            ),
        }
    }
}

impl std::error::Error for ClassBuilderError {}

/// Error produced while normalizing non-class descriptor metadata into an install plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectBuilderError {
    /// The builder received a binding for a different installation target.
    InvalidBindingTarget {
        expected_target: NativeBindingTarget,
        actual_target: NativeBindingTarget,
        js_name: Box<str>,
    },
    /// Constructor metadata is only valid for the dedicated class constructor slot.
    ConstructorBindingNotAllowed {
        target: NativeBindingTarget,
        js_name: Box<str>,
    },
    /// Two members attempted to claim the same property.
    DuplicateMember {
        target: NativeBindingTarget,
        js_name: Box<str>,
    },
    /// A getter or setter conflicted with an already-declared method.
    ConflictingMemberKinds {
        target: NativeBindingTarget,
        js_name: Box<str>,
    },
    /// An accessor declared more than one getter on the same property.
    DuplicateGetter {
        target: NativeBindingTarget,
        js_name: Box<str>,
    },
    /// An accessor declared more than one setter on the same property.
    DuplicateSetter {
        target: NativeBindingTarget,
        js_name: Box<str>,
    },
}

impl core::fmt::Display for ObjectBuilderError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::InvalidBindingTarget {
                expected_target,
                actual_target,
                js_name,
            } => write!(
                f,
                "binding '{js_name}' targets {actual_target:?}, but this builder expects {expected_target:?}"
            ),
            Self::ConstructorBindingNotAllowed { target, js_name } => write!(
                f,
                "binding '{js_name}' uses constructor slot metadata on target {target:?}"
            ),
            Self::DuplicateMember { target, js_name } => {
                write!(f, "target {target:?} already defines member '{js_name}'")
            }
            Self::ConflictingMemberKinds { target, js_name } => write!(
                f,
                "target {target:?} mixes method and accessor metadata for '{js_name}'"
            ),
            Self::DuplicateGetter { target, js_name } => {
                write!(
                    f,
                    "target {target:?} already defines a getter for '{js_name}'"
                )
            }
            Self::DuplicateSetter { target, js_name } => {
                write!(
                    f,
                    "target {target:?} already defines a setter for '{js_name}'"
                )
            }
        }
    }
}

impl std::error::Error for ObjectBuilderError {}

/// Error produced while normalizing descriptors for one host-owned object surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BurrowBuilderError {
    /// Constructor metadata is not valid on host-owned object members.
    ConstructorDescriptorNotAllowed { js_name: Box<str> },
    /// Two members attempted to claim the same property.
    DuplicateMember { js_name: Box<str> },
    /// A getter or setter conflicted with an already-declared method.
    ConflictingMemberKinds { js_name: Box<str> },
    /// An accessor declared more than one getter on the same property.
    DuplicateGetter { js_name: Box<str> },
    /// An accessor declared more than one setter on the same property.
    DuplicateSetter { js_name: Box<str> },
}

impl core::fmt::Display for BurrowBuilderError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ConstructorDescriptorNotAllowed { js_name } => {
                write!(
                    f,
                    "host-owned object surface cannot use constructor metadata for '{js_name}'"
                )
            }
            Self::DuplicateMember { js_name } => {
                write!(
                    f,
                    "host-owned object surface already defines member '{js_name}'"
                )
            }
            Self::ConflictingMemberKinds { js_name } => write!(
                f,
                "host-owned object surface mixes method and accessor metadata for '{js_name}'"
            ),
            Self::DuplicateGetter { js_name } => write!(
                f,
                "host-owned object surface already defines a getter for '{js_name}'"
            ),
            Self::DuplicateSetter { js_name } => write!(
                f,
                "host-owned object surface already defines a setter for '{js_name}'"
            ),
        }
    }
}

impl std::error::Error for BurrowBuilderError {}

/// Normalized accessor entry for future class installation.
#[derive(Clone)]
pub struct ClassAccessorPlan {
    js_name: Box<str>,
    getter: Option<NativeFunctionDescriptor>,
    setter: Option<NativeFunctionDescriptor>,
}

impl ClassAccessorPlan {
    #[must_use]
    pub fn js_name(&self) -> &str {
        &self.js_name
    }

    #[must_use]
    pub const fn getter(&self) -> Option<&NativeFunctionDescriptor> {
        self.getter.as_ref()
    }

    #[must_use]
    pub const fn setter(&self) -> Option<&NativeFunctionDescriptor> {
        self.setter.as_ref()
    }
}

/// Normalized member entry for future class installation.
#[derive(Clone)]
pub enum ClassMemberPlan {
    Method(NativeFunctionDescriptor),
    Accessor(ClassAccessorPlan),
}

impl ClassMemberPlan {
    #[must_use]
    pub fn js_name(&self) -> &str {
        match self {
            Self::Method(descriptor) => descriptor.js_name(),
            Self::Accessor(plan) => plan.js_name(),
        }
    }
}

/// Normalized accessor entry for future namespace/prototype/global installation.
#[derive(Clone)]
pub struct ObjectAccessorPlan {
    js_name: Box<str>,
    getter: Option<NativeFunctionDescriptor>,
    setter: Option<NativeFunctionDescriptor>,
}

impl ObjectAccessorPlan {
    #[must_use]
    pub fn js_name(&self) -> &str {
        &self.js_name
    }

    #[must_use]
    pub const fn getter(&self) -> Option<&NativeFunctionDescriptor> {
        self.getter.as_ref()
    }

    #[must_use]
    pub const fn setter(&self) -> Option<&NativeFunctionDescriptor> {
        self.setter.as_ref()
    }
}

/// Normalized member entry for future namespace/prototype/global installation.
#[derive(Clone)]
pub enum ObjectMemberPlan {
    Method(NativeFunctionDescriptor),
    Accessor(ObjectAccessorPlan),
}

impl ObjectMemberPlan {
    #[must_use]
    pub fn js_name(&self) -> &str {
        match self {
            Self::Method(descriptor) => descriptor.js_name(),
            Self::Accessor(plan) => plan.js_name(),
        }
    }
}

/// Validated installation plan for one non-class target object.
#[derive(Clone)]
pub struct ObjectInstallPlan {
    target: NativeBindingTarget,
    members: Vec<ObjectMemberPlan>,
}

impl ObjectInstallPlan {
    #[must_use]
    pub const fn target(&self) -> NativeBindingTarget {
        self.target
    }

    #[must_use]
    pub fn members(&self) -> &[ObjectMemberPlan] {
        &self.members
    }
}

/// Validated installation plan for one host-owned object surface.
#[derive(Clone)]
pub struct BurrowPlan {
    members: Vec<ObjectMemberPlan>,
}

impl BurrowPlan {
    #[must_use]
    pub fn members(&self) -> &[ObjectMemberPlan] {
        &self.members
    }
}

/// Validated class-installation plan derived from [`JsClassDescriptor`].
#[derive(Clone)]
pub struct ClassInstallPlan {
    js_name: Box<str>,
    constructor: Option<NativeFunctionDescriptor>,
    prototype_members: Vec<ClassMemberPlan>,
    static_members: Vec<ClassMemberPlan>,
}

/// Builder that normalizes descriptor metadata for one host-owned object surface.
#[derive(Clone, Default)]
pub struct BurrowBuilder {
    members: Vec<ObjectMemberPlan>,
}

impl BurrowBuilder {
    /// Creates an empty builder for one host-owned object surface.
    #[must_use]
    pub fn new() -> Self {
        Self {
            members: Vec::new(),
        }
    }

    /// Creates a builder from a list of function descriptors.
    pub fn from_descriptors(
        descriptors: &[NativeFunctionDescriptor],
    ) -> Result<Self, BurrowBuilderError> {
        let mut builder = Self::new();
        for descriptor in descriptors {
            builder.absorb_descriptor(descriptor)?;
        }
        Ok(builder)
    }

    /// Absorbs one function descriptor into the builder.
    pub fn absorb_descriptor(
        &mut self,
        descriptor: &NativeFunctionDescriptor,
    ) -> Result<(), BurrowBuilderError> {
        match descriptor.slot_kind() {
            NativeSlotKind::Method => {
                push_burrow_method_member(&mut self.members, descriptor.clone())
            }
            NativeSlotKind::Getter => {
                push_burrow_accessor_member(&mut self.members, descriptor.clone(), true)
            }
            NativeSlotKind::Setter => {
                push_burrow_accessor_member(&mut self.members, descriptor.clone(), false)
            }
            NativeSlotKind::Constructor => {
                Err(BurrowBuilderError::ConstructorDescriptorNotAllowed {
                    js_name: descriptor.js_name().into(),
                })
            }
        }
    }

    /// Finalizes the normalized host-object install plan.
    #[must_use]
    pub fn build(self) -> BurrowPlan {
        BurrowPlan {
            members: self.members,
        }
    }
}

impl ClassInstallPlan {
    #[must_use]
    pub fn js_name(&self) -> &str {
        &self.js_name
    }

    #[must_use]
    pub const fn constructor(&self) -> Option<&NativeFunctionDescriptor> {
        self.constructor.as_ref()
    }

    #[must_use]
    pub fn prototype_members(&self) -> &[ClassMemberPlan] {
        &self.prototype_members
    }

    #[must_use]
    pub fn static_members(&self) -> &[ClassMemberPlan] {
        &self.static_members
    }
}

/// Builder that normalizes macro-emitted class metadata into one install plan.
#[derive(Clone)]
pub struct ClassBuilder {
    js_name: Box<str>,
    constructor: Option<NativeFunctionDescriptor>,
    prototype_members: Vec<ClassMemberPlan>,
    static_members: Vec<ClassMemberPlan>,
}

impl ClassBuilder {
    /// Creates an empty builder for one JS-visible class name.
    #[must_use]
    pub fn new(js_name: impl Into<Box<str>>) -> Self {
        Self {
            js_name: js_name.into(),
            constructor: None,
            prototype_members: Vec::new(),
            static_members: Vec::new(),
        }
    }

    /// Creates a builder from macro-emitted class metadata.
    pub fn from_descriptor(descriptor: &JsClassDescriptor) -> Result<Self, ClassBuilderError> {
        let mut builder = Self::new(descriptor.js_name());
        builder.absorb_descriptor(descriptor)?;
        Ok(builder)
    }

    /// Absorbs one class descriptor into the builder.
    pub fn absorb_descriptor(
        &mut self,
        descriptor: &JsClassDescriptor,
    ) -> Result<(), ClassBuilderError> {
        if let Some(constructor) = descriptor.constructor() {
            if constructor.slot_kind() != NativeSlotKind::Constructor {
                return Err(ClassBuilderError::InvalidConstructorDescriptor {
                    slot_kind: constructor.slot_kind(),
                });
            }
            self.constructor = Some(constructor.clone());
        }

        for binding in descriptor.bindings() {
            self.push_binding(binding)?;
        }

        Ok(())
    }

    /// Finalizes the normalized install plan.
    #[must_use]
    pub fn build(self) -> ClassInstallPlan {
        ClassInstallPlan {
            js_name: self.js_name,
            constructor: self.constructor,
            prototype_members: self.prototype_members,
            static_members: self.static_members,
        }
    }

    fn push_binding(&mut self, binding: &NativeBindingDescriptor) -> Result<(), ClassBuilderError> {
        let target = binding.target();
        let function = binding.function().clone();
        let js_name = function.js_name().into();

        let members = match target {
            NativeBindingTarget::Prototype => &mut self.prototype_members,
            NativeBindingTarget::Constructor => &mut self.static_members,
            NativeBindingTarget::Namespace | NativeBindingTarget::Global => {
                return Err(ClassBuilderError::InvalidBindingTarget { target, js_name });
            }
        };

        match function.slot_kind() {
            NativeSlotKind::Method => push_method_member(members, function, target),
            NativeSlotKind::Getter => push_accessor_member(members, function, target, true),
            NativeSlotKind::Setter => push_accessor_member(members, function, target, false),
            NativeSlotKind::Constructor => {
                Err(ClassBuilderError::ConstructorBindingNotAllowed { target, js_name })
            }
        }
    }
}

#[derive(Clone)]
struct ObjectBuilder {
    target: NativeBindingTarget,
    members: Vec<ObjectMemberPlan>,
}

impl ObjectBuilder {
    fn new(target: NativeBindingTarget) -> Self {
        Self {
            target,
            members: Vec::new(),
        }
    }

    fn absorb_binding(
        &mut self,
        binding: &NativeBindingDescriptor,
    ) -> Result<(), ObjectBuilderError> {
        let actual_target = binding.target();
        let function = binding.function().clone();

        if actual_target != self.target {
            return Err(ObjectBuilderError::InvalidBindingTarget {
                expected_target: self.target,
                actual_target,
                js_name: function.js_name().into(),
            });
        }

        match function.slot_kind() {
            NativeSlotKind::Method => {
                push_object_method_member(&mut self.members, function, self.target)
            }
            NativeSlotKind::Getter => {
                push_object_accessor_member(&mut self.members, function, self.target, true)
            }
            NativeSlotKind::Setter => {
                push_object_accessor_member(&mut self.members, function, self.target, false)
            }
            NativeSlotKind::Constructor => Err(ObjectBuilderError::ConstructorBindingNotAllowed {
                target: self.target,
                js_name: function.js_name().into(),
            }),
        }
    }

    fn build(self) -> ObjectInstallPlan {
        ObjectInstallPlan {
            target: self.target,
            members: self.members,
        }
    }
}

macro_rules! define_object_builder {
    ($name:ident, $target:expr) => {
        /// Builder that normalizes native descriptors for one installation target.
        #[derive(Clone)]
        pub struct $name {
            inner: ObjectBuilder,
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl $name {
            /// Creates an empty builder for this installation target.
            #[must_use]
            pub fn new() -> Self {
                Self {
                    inner: ObjectBuilder::new($target),
                }
            }

            /// Creates a builder from a list of binding descriptors.
            pub fn from_bindings(
                bindings: &[NativeBindingDescriptor],
            ) -> Result<Self, ObjectBuilderError> {
                let mut builder = Self::new();
                for binding in bindings {
                    builder.absorb_binding(binding)?;
                }
                Ok(builder)
            }

            /// Absorbs one binding descriptor into the builder.
            pub fn absorb_binding(
                &mut self,
                binding: &NativeBindingDescriptor,
            ) -> Result<(), ObjectBuilderError> {
                self.inner.absorb_binding(binding)
            }

            /// Finalizes the normalized install plan.
            #[must_use]
            pub fn build(self) -> ObjectInstallPlan {
                self.inner.build()
            }
        }
    };
}

define_object_builder!(PrototypeBuilder, NativeBindingTarget::Prototype);
define_object_builder!(ConstructorBuilder, NativeBindingTarget::Constructor);
define_object_builder!(NamespaceBuilder, NativeBindingTarget::Namespace);
define_object_builder!(GlobalBuilder, NativeBindingTarget::Global);

fn push_method_member(
    members: &mut Vec<ClassMemberPlan>,
    function: NativeFunctionDescriptor,
    target: NativeBindingTarget,
) -> Result<(), ClassBuilderError> {
    if members
        .iter()
        .any(|member| member.js_name() == function.js_name())
    {
        return Err(ClassBuilderError::DuplicateMember {
            target,
            js_name: function.js_name().into(),
        });
    }

    members.push(ClassMemberPlan::Method(function));
    Ok(())
}

fn push_accessor_member(
    members: &mut Vec<ClassMemberPlan>,
    function: NativeFunctionDescriptor,
    target: NativeBindingTarget,
    is_getter: bool,
) -> Result<(), ClassBuilderError> {
    if let Some(member) = members
        .iter_mut()
        .find(|member| member.js_name() == function.js_name())
    {
        let ClassMemberPlan::Accessor(plan) = member else {
            return Err(ClassBuilderError::ConflictingMemberKinds {
                target,
                js_name: function.js_name().into(),
            });
        };

        if is_getter {
            if plan.getter.is_some() {
                return Err(ClassBuilderError::DuplicateGetter {
                    target,
                    js_name: function.js_name().into(),
                });
            }
            plan.getter = Some(function);
        } else {
            if plan.setter.is_some() {
                return Err(ClassBuilderError::DuplicateSetter {
                    target,
                    js_name: function.js_name().into(),
                });
            }
            plan.setter = Some(function);
        }

        return Ok(());
    }

    members.push(ClassMemberPlan::Accessor(ClassAccessorPlan {
        js_name: function.js_name().into(),
        getter: is_getter.then_some(function.clone()),
        setter: (!is_getter).then_some(function),
    }));
    Ok(())
}

fn push_object_method_member(
    members: &mut Vec<ObjectMemberPlan>,
    function: NativeFunctionDescriptor,
    target: NativeBindingTarget,
) -> Result<(), ObjectBuilderError> {
    if members
        .iter()
        .any(|member| member.js_name() == function.js_name())
    {
        return Err(ObjectBuilderError::DuplicateMember {
            target,
            js_name: function.js_name().into(),
        });
    }

    members.push(ObjectMemberPlan::Method(function));
    Ok(())
}

fn push_object_accessor_member(
    members: &mut Vec<ObjectMemberPlan>,
    function: NativeFunctionDescriptor,
    target: NativeBindingTarget,
    is_getter: bool,
) -> Result<(), ObjectBuilderError> {
    if let Some(member) = members
        .iter_mut()
        .find(|member| member.js_name() == function.js_name())
    {
        let ObjectMemberPlan::Accessor(plan) = member else {
            return Err(ObjectBuilderError::ConflictingMemberKinds {
                target,
                js_name: function.js_name().into(),
            });
        };

        if is_getter {
            if plan.getter.is_some() {
                return Err(ObjectBuilderError::DuplicateGetter {
                    target,
                    js_name: function.js_name().into(),
                });
            }
            plan.getter = Some(function);
        } else {
            if plan.setter.is_some() {
                return Err(ObjectBuilderError::DuplicateSetter {
                    target,
                    js_name: function.js_name().into(),
                });
            }
            plan.setter = Some(function);
        }

        return Ok(());
    }

    members.push(ObjectMemberPlan::Accessor(ObjectAccessorPlan {
        js_name: function.js_name().into(),
        getter: is_getter.then_some(function.clone()),
        setter: (!is_getter).then_some(function),
    }));
    Ok(())
}

fn push_burrow_method_member(
    members: &mut Vec<ObjectMemberPlan>,
    function: NativeFunctionDescriptor,
) -> Result<(), BurrowBuilderError> {
    if members
        .iter()
        .any(|member| member.js_name() == function.js_name())
    {
        return Err(BurrowBuilderError::DuplicateMember {
            js_name: function.js_name().into(),
        });
    }

    members.push(ObjectMemberPlan::Method(function));
    Ok(())
}

fn push_burrow_accessor_member(
    members: &mut Vec<ObjectMemberPlan>,
    function: NativeFunctionDescriptor,
    is_getter: bool,
) -> Result<(), BurrowBuilderError> {
    if let Some(member) = members
        .iter_mut()
        .find(|member| member.js_name() == function.js_name())
    {
        let ObjectMemberPlan::Accessor(plan) = member else {
            return Err(BurrowBuilderError::ConflictingMemberKinds {
                js_name: function.js_name().into(),
            });
        };

        if is_getter {
            if plan.getter.is_some() {
                return Err(BurrowBuilderError::DuplicateGetter {
                    js_name: function.js_name().into(),
                });
            }
            plan.getter = Some(function);
        } else {
            if plan.setter.is_some() {
                return Err(BurrowBuilderError::DuplicateSetter {
                    js_name: function.js_name().into(),
                });
            }
            plan.setter = Some(function);
        }

        return Ok(());
    }

    members.push(ObjectMemberPlan::Accessor(ObjectAccessorPlan {
        js_name: function.js_name().into(),
        getter: is_getter.then_some(function.clone()),
        setter: (!is_getter).then_some(function),
    }));
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::descriptors::{
        JsClassDescriptor, NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor,
        NativeSlotKind, VmNativeFunction,
    };
    use crate::value::RegisterValue;

    use super::{
        BurrowBuilder, BurrowBuilderError, ClassBuilder, ClassBuilderError, ClassMemberPlan,
        ConstructorBuilder, GlobalBuilder, NamespaceBuilder, ObjectBuilderError, ObjectMemberPlan,
        PrototypeBuilder,
    };

    fn passthrough_callback() -> VmNativeFunction {
        passthrough
    }

    fn passthrough(
        this: &RegisterValue,
        _args: &[RegisterValue],
        _runtime: &mut crate::interpreter::RuntimeState,
    ) -> Result<RegisterValue, crate::descriptors::VmNativeCallError> {
        Ok(*this)
    }

    #[test]
    fn class_builder_maps_descriptor_into_install_plan() {
        let descriptor = JsClassDescriptor::new("AbortController")
            .with_constructor(NativeFunctionDescriptor::constructor(
                "AbortController",
                0,
                passthrough_callback(),
            ))
            .with_binding(NativeBindingDescriptor::new(
                NativeBindingTarget::Prototype,
                NativeFunctionDescriptor::getter("signal", passthrough_callback()),
            ))
            .with_binding(NativeBindingDescriptor::new(
                NativeBindingTarget::Prototype,
                NativeFunctionDescriptor::method("abort", 0, passthrough_callback()),
            ))
            .with_binding(NativeBindingDescriptor::new(
                NativeBindingTarget::Constructor,
                NativeFunctionDescriptor::method("timeout", 1, passthrough_callback()),
            ))
            .with_binding(NativeBindingDescriptor::new(
                NativeBindingTarget::Prototype,
                NativeFunctionDescriptor::setter("onabort", passthrough_callback()),
            ))
            .with_binding(NativeBindingDescriptor::new(
                NativeBindingTarget::Prototype,
                NativeFunctionDescriptor::getter("onabort", passthrough_callback()),
            ));

        let plan = ClassBuilder::from_descriptor(&descriptor)
            .expect("descriptor should normalize")
            .build();

        assert_eq!(plan.js_name(), "AbortController");
        assert_eq!(
            plan.constructor().map(NativeFunctionDescriptor::slot_kind),
            Some(NativeSlotKind::Constructor)
        );
        assert_eq!(plan.prototype_members().len(), 3);
        assert_eq!(plan.static_members().len(), 1);

        match &plan.prototype_members()[0] {
            ClassMemberPlan::Accessor(accessor) => {
                assert_eq!(accessor.js_name(), "signal");
                assert!(accessor.getter().is_some());
                assert!(accessor.setter().is_none());
            }
            ClassMemberPlan::Method(_) => panic!("expected accessor"),
        }

        match &plan.prototype_members()[2] {
            ClassMemberPlan::Accessor(accessor) => {
                assert_eq!(accessor.js_name(), "onabort");
                assert!(accessor.getter().is_some());
                assert!(accessor.setter().is_some());
            }
            ClassMemberPlan::Method(_) => panic!("expected accessor"),
        }

        match &plan.static_members()[0] {
            ClassMemberPlan::Method(method) => assert_eq!(method.js_name(), "timeout"),
            ClassMemberPlan::Accessor(_) => panic!("expected method"),
        }
    }

    #[test]
    fn class_builder_rejects_non_class_targets() {
        let descriptor =
            JsClassDescriptor::new("Thing").with_binding(NativeBindingDescriptor::new(
                NativeBindingTarget::Namespace,
                NativeFunctionDescriptor::method("from", 1, passthrough_callback()),
            ));

        let error = match ClassBuilder::from_descriptor(&descriptor) {
            Ok(_) => panic!("target should fail"),
            Err(error) => error,
        };
        assert_eq!(
            error,
            ClassBuilderError::InvalidBindingTarget {
                target: NativeBindingTarget::Namespace,
                js_name: "from".into(),
            }
        );
    }

    #[test]
    fn class_builder_rejects_conflicting_member_kinds() {
        let descriptor = JsClassDescriptor::new("Thing")
            .with_binding(NativeBindingDescriptor::new(
                NativeBindingTarget::Prototype,
                NativeFunctionDescriptor::method("value", 0, passthrough_callback()),
            ))
            .with_binding(NativeBindingDescriptor::new(
                NativeBindingTarget::Prototype,
                NativeFunctionDescriptor::getter("value", passthrough_callback()),
            ));

        let error = match ClassBuilder::from_descriptor(&descriptor) {
            Ok(_) => panic!("conflict should fail"),
            Err(error) => error,
        };
        assert_eq!(
            error,
            ClassBuilderError::ConflictingMemberKinds {
                target: NativeBindingTarget::Prototype,
                js_name: "value".into(),
            }
        );
    }

    #[test]
    fn class_builder_rejects_duplicate_accessor_halves() {
        let descriptor = JsClassDescriptor::new("Thing")
            .with_binding(NativeBindingDescriptor::new(
                NativeBindingTarget::Prototype,
                NativeFunctionDescriptor::getter("value", passthrough_callback()),
            ))
            .with_binding(NativeBindingDescriptor::new(
                NativeBindingTarget::Prototype,
                NativeFunctionDescriptor::getter("value", passthrough_callback()),
            ));

        let error = match ClassBuilder::from_descriptor(&descriptor) {
            Ok(_) => panic!("duplicate getter should fail"),
            Err(error) => error,
        };
        assert_eq!(
            error,
            ClassBuilderError::DuplicateGetter {
                target: NativeBindingTarget::Prototype,
                js_name: "value".into(),
            }
        );
    }

    #[test]
    fn class_builder_keeps_native_callbacks_intact() {
        let descriptor =
            JsClassDescriptor::new("Counter").with_binding(NativeBindingDescriptor::new(
                NativeBindingTarget::Prototype,
                NativeFunctionDescriptor::method("valueOf", 0, passthrough_callback()),
            ));

        let plan = ClassBuilder::from_descriptor(&descriptor)
            .expect("descriptor should normalize")
            .build();

        let ClassMemberPlan::Method(method) = &plan.prototype_members()[0] else {
            panic!("expected method");
        };

        let value = (method.callback())(&RegisterValue::from_i32(7), &[], &mut Default::default())
            .expect("callback should succeed");
        assert_eq!(value, RegisterValue::from_i32(7));
    }

    #[test]
    fn namespace_builder_maps_bindings_into_install_plan() {
        let bindings = vec![
            NativeBindingDescriptor::new(
                NativeBindingTarget::Namespace,
                NativeFunctionDescriptor::method("abs", 1, passthrough_callback()),
            ),
            NativeBindingDescriptor::new(
                NativeBindingTarget::Namespace,
                NativeFunctionDescriptor::getter("mode", passthrough_callback()),
            ),
            NativeBindingDescriptor::new(
                NativeBindingTarget::Namespace,
                NativeFunctionDescriptor::setter("mode", passthrough_callback()),
            ),
        ];

        let plan = NamespaceBuilder::from_bindings(&bindings)
            .expect("bindings should normalize")
            .build();

        assert_eq!(plan.target(), NativeBindingTarget::Namespace);
        assert_eq!(plan.members().len(), 2);

        match &plan.members()[0] {
            ObjectMemberPlan::Method(method) => assert_eq!(method.js_name(), "abs"),
            ObjectMemberPlan::Accessor(_) => panic!("expected method"),
        }

        match &plan.members()[1] {
            ObjectMemberPlan::Accessor(accessor) => {
                assert_eq!(accessor.js_name(), "mode");
                assert!(accessor.getter().is_some());
                assert!(accessor.setter().is_some());
            }
            ObjectMemberPlan::Method(_) => panic!("expected accessor"),
        }
    }

    #[test]
    fn prototype_builder_rejects_wrong_target() {
        let bindings = vec![NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("from", 1, passthrough_callback()),
        )];

        let error = match PrototypeBuilder::from_bindings(&bindings) {
            Ok(_) => panic!("target should fail"),
            Err(error) => error,
        };
        assert_eq!(
            error,
            ObjectBuilderError::InvalidBindingTarget {
                expected_target: NativeBindingTarget::Prototype,
                actual_target: NativeBindingTarget::Constructor,
                js_name: "from".into(),
            }
        );
    }

    #[test]
    fn constructor_builder_rejects_constructor_slot_metadata() {
        let bindings = vec![NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::constructor("Thing", 0, passthrough_callback()),
        )];

        let error = match ConstructorBuilder::from_bindings(&bindings) {
            Ok(_) => panic!("constructor slot should fail"),
            Err(error) => error,
        };
        assert_eq!(
            error,
            ObjectBuilderError::ConstructorBindingNotAllowed {
                target: NativeBindingTarget::Constructor,
                js_name: "Thing".into(),
            }
        );
    }

    #[test]
    fn global_builder_keeps_native_callbacks_intact() {
        let bindings = vec![NativeBindingDescriptor::new(
            NativeBindingTarget::Global,
            NativeFunctionDescriptor::method("parseInt", 1, passthrough_callback()),
        )];

        let plan = GlobalBuilder::from_bindings(&bindings)
            .expect("bindings should normalize")
            .build();

        let ObjectMemberPlan::Method(method) = &plan.members()[0] else {
            panic!("expected method");
        };

        let value = (method.callback())(&RegisterValue::from_i32(9), &[], &mut Default::default())
            .expect("callback should succeed");
        assert_eq!(value, RegisterValue::from_i32(9));
    }

    #[test]
    fn burrow_builder_maps_descriptors_into_host_object_plan() {
        let descriptors = vec![
            NativeFunctionDescriptor::method("set", 2, passthrough_callback()),
            NativeFunctionDescriptor::getter("size", passthrough_callback()),
            NativeFunctionDescriptor::setter("size", passthrough_callback()),
        ];

        let plan = BurrowBuilder::from_descriptors(&descriptors)
            .expect("descriptors should normalize")
            .build();

        assert_eq!(plan.members().len(), 2);

        match &plan.members()[0] {
            ObjectMemberPlan::Method(method) => assert_eq!(method.js_name(), "set"),
            ObjectMemberPlan::Accessor(_) => panic!("expected method"),
        }

        match &plan.members()[1] {
            ObjectMemberPlan::Accessor(accessor) => {
                assert_eq!(accessor.js_name(), "size");
                assert!(accessor.getter().is_some());
                assert!(accessor.setter().is_some());
            }
            ObjectMemberPlan::Method(_) => panic!("expected accessor"),
        }
    }

    #[test]
    fn burrow_builder_rejects_constructor_metadata() {
        let descriptors = vec![NativeFunctionDescriptor::constructor(
            "Thing",
            0,
            passthrough_callback(),
        )];

        let error = match BurrowBuilder::from_descriptors(&descriptors) {
            Ok(_) => panic!("constructor slot should fail"),
            Err(error) => error,
        };
        assert_eq!(
            error,
            BurrowBuilderError::ConstructorDescriptorNotAllowed {
                js_name: "Thing".into(),
            }
        );
    }
}
