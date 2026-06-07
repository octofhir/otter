//! `ClassConstructor` — runtime carrier for `class C { … }`.
//!
//! Each class declaration / expression evaluates to a
//! [`ClassConstructor`] value: a `Copy` 4-byte wrapper over a
//! `Gc<ClassConstructorBody>`. The body holds the callable that runs
//! for `new C(...)` / `super(...)`, the instance prototype every
//! `new C(...)` inherits from, and the static-side object that owns
//! `C.foo` static methods and chains through `extends` for static
//! inheritance.
//!
//! # Contents
//! - [`ClassConstructorBody`] — GC payload (callable + prototype +
//!   statics).
//! - [`ClassConstructor`] — `Copy` wrapper handle.
//! - [`CLASS_CONSTRUCTOR_BODY_TYPE_TAG`] — type-tag byte stored in the
//!   GC header for runtime discrimination.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-class-definitions>
//! - <https://tc39.es/ecma262/#sec-runtime-semantics-classdefinitionevaluation>

use otter_gc::raw::RawGc;
use otter_macros::Pelt;

use crate::Value;
use crate::object::JsObject;

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`ClassConstructorBody`].
pub const CLASS_CONSTRUCTOR_BODY_TYPE_TAG: u8 = 0x1f;

/// GC-allocated payload backing every [`Value::ClassConstructor`].
/// Holds the callable, the instance prototype, and the static-side
/// object the class exposes.
#[derive(Debug, Pelt)]
#[pelt(tag = CLASS_CONSTRUCTOR_BODY_TYPE_TAG)]
pub struct ClassConstructorBody {
    /// The actual callable (`Value::Function` / `Value::Closure` /
    /// `Value::NativeFunction`) the runtime invokes for `new C(...)`
    /// or `super(...)`. Constructed by the compiler's class-lowering
    /// pass.
    pub ctor: Value,
    /// `C.prototype` — every instance built by `new C(...)` inherits
    /// from this object, and instance methods live here.
    pub prototype: JsObject,
    /// Static side: own static methods/properties live here, and when
    /// `class D extends C` the static object's `[[Prototype]]` chains
    /// to `C`'s static object so static inheritance just falls out of
    /// the existing prototype walker.
    pub statics: JsObject,
    /// The constructor's own [[Prototype]] identity — the parent
    /// class value for `class D extends C`, `null` for
    /// `extends null`, `undefined` for a base class
    /// (= %Function.prototype% semantics). The statics object's own
    /// prototype keeps the parallel walk-able chain; this slot
    /// preserves identity for getPrototypeOf / super-base checks.
    pub ctor_proto: Value,
}

/// Cheap-to-clone class-constructor handle. Wraps a
/// `Gc<ClassConstructorBody>` so `Value::ClassConstructor` stays a
/// 4-byte payload and the underlying body is GC-managed.
#[derive(Clone, Copy, Debug)]
#[repr(transparent)]
pub struct ClassConstructor {
    inner: otter_gc::Gc<ClassConstructorBody>,
}

impl ClassConstructor {
    /// Allocate a class constructor while exposing caller-owned roots
    /// across the body allocation.
    pub(crate) fn new_with_roots(
        heap: &mut otter_gc::GcHeap,
        ctor: Value,
        prototype: JsObject,
        statics: JsObject,
        external_visit: &mut otter_gc::heap::RootSlotVisitor<'_>,
    ) -> Result<Self, otter_gc::OutOfMemory> {
        let prototype_root = Value::object(prototype);
        let statics_root = Value::object(statics);
        let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
            external_visit(visitor);
            ctor.trace_value_slots(visitor);
            prototype_root.trace_value_slots(visitor);
            statics_root.trace_value_slots(visitor);
        };
        Ok(Self {
            inner: heap.alloc_with_roots(
                ClassConstructorBody {
                    ctor,
                    prototype,
                    statics,
                    ctor_proto: Value::undefined(),
                },
                &mut visit,
            )?,
        })
    }

    /// Read the constructor's own [[Prototype]] identity slot.
    /// `undefined` means base class (%Function.prototype%).
    #[inline]
    #[must_use]
    pub fn ctor_proto(self, heap: &otter_gc::GcHeap) -> Value {
        heap.read_payload(self.inner, |body| body.ctor_proto)
    }

    /// Write the constructor's [[Prototype]] identity slot.
    #[inline]
    pub fn set_ctor_proto(self, heap: &mut otter_gc::GcHeap, proto: Value) {
        heap.with_payload(self.inner, |body| body.ctor_proto = proto);
        heap.record_write(self.inner, &proto);
    }

    /// Identity comparison — `===` follows the GC handle's
    /// 32-bit-offset equality.
    #[inline]
    #[must_use]
    pub fn ptr_eq(self, other: Self) -> bool {
        self.inner == other.inner
    }

    /// Read the underlying callable (Function / Closure / native).
    #[inline]
    #[must_use]
    pub fn ctor(self, heap: &otter_gc::GcHeap) -> Value {
        heap.read_payload(self.inner, |body| body.ctor)
    }

    /// Read `C.prototype`.
    #[inline]
    #[must_use]
    pub fn prototype(self, heap: &otter_gc::GcHeap) -> JsObject {
        heap.read_payload(self.inner, |body| body.prototype)
    }

    /// Read the static-side object.
    #[inline]
    #[must_use]
    pub fn statics(self, heap: &otter_gc::GcHeap) -> JsObject {
        heap.read_payload(self.inner, |body| body.statics)
    }

    /// GC root — used by VM tracing roots when a class constructor
    /// sits in a register or environment slot.
    #[doc(hidden)]
    #[inline]
    pub fn raw(self) -> RawGc {
        self.inner.raw()
    }

    /// Reinterpret a body handle as the public [`ClassConstructor`]
    /// wrapper. Used by [`crate::value::Value::as_class_constructor`]
    /// after a `GcHeader::type_tag` check has confirmed the body is a
    /// [`ClassConstructorBody`].
    #[inline]
    #[must_use]
    pub fn from_gc(inner: otter_gc::Gc<ClassConstructorBody>) -> Self {
        Self { inner }
    }
}
