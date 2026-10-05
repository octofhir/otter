//! The `Intl` namespace object's own surface: static methods
//! ([`getCanonicalLocales`](crate::intl::supported::get_canonical_locales))
//! and well-known-symbol properties (`@@toStringTag`).
//!
//! Kept out of [`crate::intl::bootstrap`] so that the bootstrap install
//! path stays a flat sequence of delegations.
//!
//! # See also
//! - <https://tc39.es/ecma402/#intl-object>

use crate::js_surface::{Attr, JsSurfaceError, MethodSpec, ObjectBuilder};
use crate::native_function::NativeCall;
use crate::object::{self, JsObject};
use crate::symbol::WellKnownSymbols;
use crate::{NativeCtx, NativeError, Value};

const fn method(
    name: &'static str,
    length: u8,
    call: for<'rt> fn(&mut NativeCtx<'rt>, &[Value]) -> Result<Value, NativeError>,
) -> MethodSpec {
    MethodSpec {
        name,
        length,
        attrs: Attr::builtin_function(),
        call: NativeCall::Static(call),
    }
}

/// Static methods installed on the `Intl` namespace object.
pub const INTL_NAMESPACE_METHODS: &[MethodSpec] = &[
    method(
        "getCanonicalLocales",
        1,
        crate::intl::supported::get_canonical_locales,
    ),
    method(
        "supportedValuesOf",
        1,
        crate::intl::supported::supported_values_of,
    ),
];

/// §8.1 — install `Intl[@@toStringTag] = "Intl"` (non-enumerable,
/// configurable) on the namespace object.
pub fn install_namespace_well_knowns(
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
    well_known: &WellKnownSymbols,
) -> Result<(), JsSurfaceError> {
    let Some(intl) = object::get(global, heap, "Intl").and_then(|value| value.as_object()) else {
        return Ok(());
    };
    let mut builder =
        ObjectBuilder::from_object_with_value_roots(heap, intl, vec![Value::object(global)]);
    builder.to_string_tag(well_known, "Intl")?;
    let _current = builder.build();
    Ok(())
}
