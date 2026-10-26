//! `%Array%` constructor installer.
//!
//! Routes through `couch!`. Static methods come from
//! `ARRAY_STATIC_METHODS`, prototype methods from
//! `ARRAY_PROTOTYPE_METHODS`. The constructor body handles the
//! §23.1.1.1 "single numeric arg = pre-sized sparse array" / "n+
//! args = collected values" split internally, plus the
//! `new.target` prototype of a subclass.
//!
//! # Contents
//! - Canonical constructor, static and prototype surface installation.
//! - Array prototype birth metadata and its length descriptor.
//! - Rooted constructor arguments and `new.target` prototype lookup.
//!
//! # Invariants
//! - Only this canonical prototype birth assigns the enduring Array brand.
//! - Constructor values and prototypes stay rooted across allocation and calls.
//! - Mutable global names never select intrinsic execution semantics.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-array-constructor>

use crate::ValueWindow;
use crate::js_surface::JsSurfaceError;
use crate::object::{self, PropertyDescriptor};
use crate::{NativeCtx, NativeError, Value};

otter_macros::couch! {
    name = "Array",
    feature = CORE,
    constructor = (length = 1, call = array_ctor_call),
    static_method_specs = [crate::array_statics::ARRAY_STATIC_METHODS],
    prototype = {
        method_specs = [crate::array_prototype::ARRAY_PROTOTYPE_METHODS],
    },
    post_install = install_array_prototype_length,
}

fn install_array_prototype_length(
    heap: &mut otter_gc::GcHeap,
    _global: object::JsObject,
    ctor: crate::native_function::NativeFunction,
) -> Result<(), JsSurfaceError> {
    let Some(descriptor) = ctor
        .own_property_descriptor(&mut *heap, "prototype")
        .map_err(|_| JsSurfaceError::DefinePropertyFailed("prototype"))?
    else {
        return Err(JsSurfaceError::DefinePropertyFailed("prototype"));
    };
    let object::DescriptorKind::Data { value } = descriptor.kind else {
        return Err(JsSurfaceError::DefinePropertyFailed("prototype"));
    };
    let Some(mut prototype) = value.as_object() else {
        return Ok(());
    };
    object::mark_array_prototype(&mut prototype, heap)?;
    let desc = PropertyDescriptor::data(Value::number_i32(0), true, false, false);
    if !object::define_own_property_in_place(&mut prototype, heap, "length", desc)? {
        return Err(JsSurfaceError::DefinePropertyFailed("length"));
    }
    Ok(())
}

/// §23.1.1.1 Array(...values) — both `Array(…)` and `new Array(…)`
/// reach this callback. Single numeric argument means "pre-sized
/// sparse array of length n"; anything else collects values
/// verbatim. The prototype comes from `new.target` first (step 2),
/// whose `prototype` read may run JavaScript, so the arguments and then the
/// array stay rooted across every allocation.
///
/// <https://tc39.es/ecma262/#sec-array>
fn array_ctor_call(ctx: &mut NativeCtx<'_>, args: ValueWindow<'_>) -> Result<Value, NativeError> {
    ctx.scope(|mut scope| {
        let elements: smallvec::SmallVec<[_; 8]> = (0..args.len())
            .map(|index| scope.argument(args, index))
            .collect();
        let prototype = array_new_target_prototype(scope.context())?;
        let prototype = prototype.map(|value| scope.value(value));
        let length = match &elements[..] {
            [only] if let Some(n) = scope.raw(*only).as_number() => {
                let raw = n.as_f64();
                let len = raw as u32;
                if !raw.is_finite() || raw < 0.0 || raw != f64::from(len) {
                    return Err(NativeError::RangeError {
                        name: "Array",
                        reason: "Invalid array length".to_string(),
                    });
                }
                Some(len as usize)
            }
            _ => None,
        };
        let array = match length {
            Some(len) => scope.array_create(len, &[], prototype)?,
            None => scope.array_create(elements.len(), &elements, prototype)?,
        };
        Ok(scope.finish(array))
    })
}

/// The prototype `new.target` gives a new array (GetPrototypeFromConstructor),
/// or `None` for a call without `new.target` or one whose `prototype` is not
/// an object.
fn array_new_target_prototype(ctx: &mut NativeCtx<'_>) -> Result<Option<Value>, NativeError> {
    crate::intrinsics::shared::native_new_target_prototype(ctx, "Array")
}
