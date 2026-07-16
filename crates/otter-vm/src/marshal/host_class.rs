//! Host-class instances: branded data cells and prototype-correct
//! construction.
//!
//! A declared host class stores its Rust data inside a [`HostInstance`]
//! cell rather than a bare `Box<dyn Any>`. The cell carries a
//! monomorphized ancestry caster, so a method declared on a base class
//! (`Blob.prototype.slice`) can view the base data inside a subclass
//! instance (`File`) without any global registry — the cast walk is a
//! per-class `fn` pointer generated at the declaration site.
//!
//! Construction goes through [`construct_instance`], which implements
//! the `GetPrototypeFromConstructor` shape for native constructors that
//! build their own instance: the prototype comes from
//! `new.target.prototype` when the call is a construct call (so JS
//! subclasses are linked correctly by construction), falling back to
//! the named class's registered prototype.
//!
//! # Contents
//! - [`HostInstance`] — the branded data cell + ancestry caster.
//! - [`HostAncestry`] — implemented (usually by macro) per class.
//! - [`construct_instance`] — prototype-correct instance building.
//! - [`host_data_view`] — brand-checked reads used by
//!   [`super::HostRef`] and receiver glue.
//!
//! # Invariants
//! - The caster is a plain `fn` pointer: no captures, no per-isolate
//!   state, no `thread_local`.
//! - `host_data_view::<T>` accepts both a [`HostInstance`] whose
//!   ancestry contains `T` and a legacy bare-`T` host object, so
//!   migrated and unmigrated classes interoperate during the
//!   transition.
//! - The prototype is resolved *before* the caller-provided data is
//!   installed, matching `OrdinaryCreateFromConstructor` ordering as
//!   closely as a self-allocating constructor can.
//!
//! # See also
//! - [`crate::object`] — host-data storage on exotic slots.
//! - `EXTENSION_API_PLAN.md` §3.2–§3.4 — the design.

use std::any::{Any, TypeId};

use crate::handles::Local;
use crate::{Value, object};

use super::cx::MarshalCx;
use super::error::JsError;

/// Ancestry hook for declared host classes. The declaration macro
/// implements this; a standalone class uses the default (self-only)
/// walk. `ancestor` must return a view into `self`, never a different
/// allocation.
pub trait HostAncestry: Any + Sized {
    /// View `self` as the ancestor with the given `TypeId`, walking
    /// the parent chain. The default covers the class itself.
    fn ancestor(&self, target: TypeId) -> Option<&dyn Any> {
        if target == TypeId::of::<Self>() {
            Some(self)
        } else {
            None
        }
    }

    /// Mutable counterpart of [`Self::ancestor`].
    fn ancestor_mut(&mut self, target: TypeId) -> Option<&mut dyn Any> {
        if target == TypeId::of::<Self>() {
            Some(self)
        } else {
            None
        }
    }
}

fn cast_thunk<T: HostAncestry>(any: &dyn Any, target: TypeId) -> Option<&dyn Any> {
    any.downcast_ref::<T>()?.ancestor(target)
}

fn cast_mut_thunk<T: HostAncestry>(any: &mut dyn Any, target: TypeId) -> Option<&mut dyn Any> {
    any.downcast_mut::<T>()?.ancestor_mut(target)
}

/// The branded data cell a declared host class stores in its
/// instance's host slot.
pub struct HostInstance {
    data: Box<dyn Any>,
    cast: for<'a> fn(&'a dyn Any, TypeId) -> Option<&'a dyn Any>,
    cast_mut: for<'a> fn(&'a mut dyn Any, TypeId) -> Option<&'a mut dyn Any>,
    class_name: &'static str,
}

impl std::fmt::Debug for HostInstance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostInstance")
            .field("class_name", &self.class_name)
            .finish_non_exhaustive()
    }
}

impl HostInstance {
    /// Wrap class data in a branded cell.
    #[must_use]
    pub fn new<T: HostAncestry>(class_name: &'static str, data: T) -> Self {
        Self {
            data: Box::new(data),
            cast: cast_thunk::<T>,
            cast_mut: cast_mut_thunk::<T>,
            class_name,
        }
    }

    /// The declared JS class name of the stored data.
    #[must_use]
    pub fn class_name(&self) -> &'static str {
        self.class_name
    }

    /// View the stored data as ancestor type `T`, walking the class's
    /// declared parent chain.
    #[must_use]
    pub fn view<T: Any>(&self) -> Option<&T> {
        (self.cast)(self.data.as_ref(), TypeId::of::<T>())?.downcast_ref::<T>()
    }

    /// Mutable counterpart of [`Self::view`].
    #[must_use]
    pub fn view_mut<T: Any>(&mut self) -> Option<&mut T> {
        (self.cast_mut)(self.data.as_mut(), TypeId::of::<T>())?.downcast_mut::<T>()
    }
}

/// Brand-checked read of host-class data of type `T` behind a scope
/// handle. Accepts both declared-class instances ([`HostInstance`]
/// cells, ancestry-aware) and legacy bare-`T` host objects.
pub(super) fn host_data_view<T: Any, R>(
    cx: &MarshalCx<'_, '_, '_>,
    v: Local<'_>,
    f: impl FnOnce(&T) -> R,
) -> Result<R, JsError> {
    host_data_view_raw(cx.escape(v), cx.heap(), f).map_err(JsError::Type)
}

/// Brand-checked host-data read shared by the marshalling and native-scope
/// surfaces. The callback runs while the GC payload is borrowed and therefore
/// must not allocate JavaScript values or re-enter the VM.
pub(crate) fn host_data_view_raw<T: Any, R>(
    raw: Value,
    heap: &otter_gc::GcHeap,
    f: impl FnOnce(&T) -> R,
) -> Result<R, String> {
    let Some(object) = raw.as_object() else {
        return Err("value is not an object".to_string());
    };
    // Declared-class path: the cell knows its ancestry. `f` is consumed
    // by whichever branch runs, so thread it through the probe.
    let mut f = Some(f);
    let cell_result = object::with_host_data::<HostInstance, _>(object, heap, |cell| {
        cell.view::<T>()
            .map(|data| (f.take().expect("single call"))(data))
    });
    match cell_result {
        Ok(Some(result)) => return Ok(result),
        Ok(None) => {
            return Err("receiver is an instance of an unrelated class".to_string());
        }
        Err(_) => {}
    }
    let f = f.take().expect("closure unconsumed on the legacy path");
    // Legacy path: bare host data of exactly `T`.
    object::with_host_data::<T, R>(object, heap, f).map_err(|err| err.to_string())
}

/// Build a host-class instance the way `new <Class>` must: data in a
/// branded [`HostInstance`] cell, prototype from
/// `new.target.prototype` when constructing (falling back to the named
/// class's prototype), parked in the ambient scope.
///
/// This is the construction path for declared classes and the
/// `IntoJs`-for-host-class lowering; it is what makes `instanceof`,
/// inherited prototype methods, and JS subclass linkage
/// (`class F extends Blob { … super(…) … }`) hold with no manual
/// prototype work at the declaration site.
pub fn construct_instance<'s, T: HostAncestry>(
    cx: &mut MarshalCx<'_, '_, 's>,
    class_name: &'static str,
    data: T,
) -> Result<Local<'s>, JsError> {
    // Resolve the prototype before installing the data, mirroring
    // OrdinaryCreateFromConstructor ordering. A construct call honors
    // `new.target.prototype` (JS subclasses); everything else — plain
    // factory calls from Rust — uses the registered class prototype.
    let proto = prototype_for_construction(cx, class_name);
    let instance = cx
        .ctx()
        .alloc_host_object(HostInstance::new(class_name, data))
        .map_err(|err| JsError::Type(err.to_string()))?;
    let handle = cx.park(Value::object(instance));
    if let Some(proto) = proto {
        let raw_proto = cx.escape(proto);
        let raw_instance = cx.escape(handle);
        if let Some(object) = raw_instance.as_object() {
            object::set_prototype_value(object, cx.heap_mut(), Some(raw_proto));
        }
    }
    Ok(handle)
}

/// The prototype a fresh instance of `class_name` must carry, parked in
/// the ambient scope: `new.target.prototype` when this is a construct
/// call and that value is an object, else the registered prototype of
/// the named class, else `None` (bootstrap-order edge: class not
/// installed yet).
fn prototype_for_construction<'s>(
    cx: &mut MarshalCx<'_, '_, 's>,
    class_name: &str,
) -> Option<Local<'s>> {
    if let Some(new_target) = cx.ctx().new_target().copied() {
        let target_handle = cx.park(new_target);
        if let Some(proto) = constructor_prototype_read(cx, target_handle)
            && proto.is_object_type()
        {
            return Some(cx.park(proto));
        }
    }
    let proto = cx.ctx().class_instance_prototype(class_name)?;
    Some(cx.park(proto))
}

/// Read `<ctor>.prototype` off an arbitrary constructor-shaped value:
/// an ordinary object, a builtin native function, or a JS
/// `class`-declared constructor (the exotic representation
/// `new.target` carries for user subclasses). The returned raw value
/// is parked by the caller before any further allocation.
fn constructor_prototype_read(cx: &mut MarshalCx<'_, '_, '_>, ctor: Local<'_>) -> Option<Value> {
    let raw = cx.escape(ctor);
    if let Some(class) = raw.as_class_constructor() {
        return Some(Value::object(class.prototype(cx.heap())));
    }
    if let Some(object) = raw.as_object() {
        return object::get(object, cx.heap(), "prototype");
    }
    if let Some(native) = raw.as_native_function() {
        let descriptor = native
            .own_property_descriptor(cx.heap_mut(), "prototype")
            .ok()
            .flatten()?;
        if let object::DescriptorKind::Data { value } = descriptor.kind {
            return Some(value);
        }
    }
    None
}

/// Build a host-class instance for a plain Rust-side return value —
/// the `IntoJs` lowering for declared classes. Unlike
/// [`construct_instance`] this never consults `new.target`: a method
/// returning an auxiliary instance (`Blob.prototype.slice` returning a
/// fresh `Blob`) must carry the class's own prototype even when it
/// runs inside somebody else's construct call.
pub fn class_instance<'s, T: HostAncestry>(
    cx: &mut MarshalCx<'_, '_, 's>,
    class_name: &'static str,
    data: T,
) -> Result<Local<'s>, JsError> {
    let proto = cx
        .ctx()
        .class_instance_prototype(class_name)
        .map(|proto| cx.park(proto));
    let instance = cx
        .ctx()
        .alloc_host_object(HostInstance::new(class_name, data))
        .map_err(|err| JsError::Type(err.to_string()))?;
    let handle = cx.park(Value::object(instance));
    if let Some(proto) = proto {
        let raw_proto = cx.escape(proto);
        let raw_instance = cx.escape(handle);
        if let Some(object) = raw_instance.as_object() {
            object::set_prototype_value(object, cx.heap_mut(), Some(raw_proto));
        }
    }
    Ok(handle)
}

/// Mutable counterpart of [`host_data_view`]: brand-checked mutable
/// access to host-class data of type `T`, ancestry-aware for declared
/// classes with a bare-`T` fallback for legacy host objects.
pub(super) fn host_data_view_mut<T: Any, R>(
    cx: &mut MarshalCx<'_, '_, '_>,
    v: Local<'_>,
    f: impl FnOnce(&mut T) -> R,
) -> Result<R, JsError> {
    let raw = cx.escape(v);
    host_data_view_raw_mut(raw, cx.heap_mut(), f).map_err(JsError::Type)
}

/// Mutable counterpart of [`host_data_view_raw`]. The callback owns the only
/// mutable payload borrow for its duration and must not allocate JavaScript
/// values or re-enter the VM.
pub(crate) fn host_data_view_raw_mut<T: Any, R>(
    raw: Value,
    heap: &mut otter_gc::GcHeap,
    f: impl FnOnce(&mut T) -> R,
) -> Result<R, String> {
    let Some(object) = raw.as_object() else {
        return Err("value is not an object".to_string());
    };
    let mut f = Some(f);
    let cell_result = object::with_host_data_mut::<HostInstance, _>(object, heap, |cell| {
        cell.view_mut::<T>()
            .map(|data| (f.take().expect("single call"))(data))
    });
    match cell_result {
        Ok(Some(result)) => return Ok(result),
        Ok(None) => {
            return Err("receiver is an instance of an unrelated class".to_string());
        }
        Err(_) => {}
    }
    let f = f.take().expect("closure unconsumed on the legacy path");
    object::with_host_data_mut::<T, R>(object, heap, f).map_err(|err| err.to_string())
}

/// Compile-time metadata a class declaration pins on its data type.
/// The declaration macro implements this; downstream declarations use
/// it to resolve a parent class's JS name (`extends = Blob` reads
/// `<Blob as HostClassMeta>::JS_NAME` instead of duplicating the
/// string).
pub trait HostClassMeta {
    /// The declared JS class name.
    const JS_NAME: &'static str;
}

/// Union-variant probe: a cheap, side-effect-free test for whether a
/// JS value can convert to `Self` — WebIDL union distinguishability
/// without trial coercion. The default accepts everything (a catch-all
/// variant like a string coercion); brand- and buffer-shaped types
/// override it.
pub trait JsUnionProbe {
    /// Whether `v` distinguishes as `Self`.
    fn probe(cx: &MarshalCx<'_, '_, '_>, v: Local<'_>) -> bool {
        let _ = (cx, v);
        true
    }
}

impl JsUnionProbe for super::BufferSource {
    fn probe(cx: &MarshalCx<'_, '_, '_>, v: Local<'_>) -> bool {
        let raw = cx.escape(v);
        raw.as_typed_array(cx.heap()).is_some() || raw.as_array_buffer().is_some()
    }
}

impl JsUnionProbe for super::USVString {}
impl JsUnionProbe for super::DOMString {}
impl JsUnionProbe for f64 {}
impl JsUnionProbe for bool {}
