//! Function-kind prototype cache for callable `@@toStringTag`.
//!
//! ECMAScript exposes generator, async, and async-generator function
//! branding through inherited prototype properties rather than through
//! `Object.prototype.toString`'s builtinTag table. This module owns
//! the small VM-side prototype graph needed for those ordinary
//! bytecode functions.
//!
//! # Contents
//! - [`FunctionKindPrototypes`] — cached constructor/prototype pairs.
//! - Post-bootstrap allocation of the prototype objects.
//! - Lookup helpers keyed by bytecode function metadata.
//!
//! # Invariants
//! - Cached objects are allocated after `%Function.prototype%` exists.
//! - All cached object handles are traced as interpreter roots.
//! - The default ordinary function kind is represented by `None`;
//!   callers should then fall back to `%Function.prototype%`.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-generatorfunction-objects>
//! - <https://tc39.es/ecma262/#sec-async-function-objects>

use crate::{
    ExecutionContext, Interpreter, JsObject, JsString, Value, gc_trace::GcTrace,
    js_surface::JsSurfaceError, object, symbol::WellKnown,
};
use otter_gc::raw::{RawGc, SlotVisitor};

#[derive(Default)]
pub(crate) struct FunctionKindPrototypes {
    generator_constructor: Option<JsObject>,
    generator_prototype: Option<JsObject>,
    async_constructor: Option<JsObject>,
    async_prototype: Option<JsObject>,
    async_generator_constructor: Option<JsObject>,
    async_generator_prototype: Option<JsObject>,
}

impl FunctionKindPrototypes {
    pub(crate) fn build_post_bootstrap(
        heap: &mut otter_gc::GcHeap,
        shape_root: object::ShapeHandle,
        function_proto: JsObject,
        well_known: &crate::symbol::WellKnownSymbols,
    ) -> Result<Self, JsSurfaceError> {
        let function_proto_value = Value::Object(function_proto);
        let tag_sym = well_known.get(WellKnown::ToStringTag);
        let mut make = |tag: &'static str| -> Result<(JsObject, JsObject), JsSurfaceError> {
            let proto = {
                let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
                    function_proto_value.trace_value_slots(visitor);
                };
                object::alloc_object_with_shape_roots(heap, shape_root, &mut visit)
                    .map_err(|_| JsSurfaceError::OutOfMemory)?
            };
            object::set_prototype(proto, heap, Some(function_proto));
            let tag_string =
                JsString::from_str(tag, heap).map_err(|_| JsSurfaceError::OutOfMemory)?;
            object::define_own_symbol_property_partial(
                proto,
                heap,
                &tag_sym,
                object::PartialPropertyDescriptor {
                    value: Some(Value::String(tag_string)),
                    writable: Some(false),
                    enumerable: Some(false),
                    configurable: Some(true),
                    ..Default::default()
                },
            );

            let proto_value = Value::Object(proto);
            let ctor = {
                let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
                    function_proto_value.trace_value_slots(visitor);
                    proto_value.trace_value_slots(visitor);
                };
                object::alloc_object_with_shape_roots(heap, shape_root, &mut visit)
                    .map_err(|_| JsSurfaceError::OutOfMemory)?
            };
            object::set_prototype(ctor, heap, Some(function_proto));
            object::define_own_property(
                ctor,
                heap,
                "prototype",
                object::PropertyDescriptor::data(proto_value, false, false, false),
            );
            object::define_own_property(
                proto,
                heap,
                "constructor",
                object::PropertyDescriptor::data(Value::Object(ctor), true, false, true),
            );
            Ok((ctor, proto))
        };

        let (generator_constructor, generator_prototype) = make("GeneratorFunction")?;
        let (async_constructor, async_prototype) = make("AsyncFunction")?;
        let (async_generator_constructor, async_generator_prototype) =
            make("AsyncGeneratorFunction")?;
        Ok(Self {
            generator_constructor: Some(generator_constructor),
            generator_prototype: Some(generator_prototype),
            async_constructor: Some(async_constructor),
            async_prototype: Some(async_prototype),
            async_generator_constructor: Some(async_generator_constructor),
            async_generator_prototype: Some(async_generator_prototype),
        })
    }

    pub(crate) fn prototype_for(
        &self,
        context: &ExecutionContext,
        function_id: u32,
    ) -> Option<JsObject> {
        match context.function(function_id) {
            Some(function) if function.is_async_generator => self.async_generator_prototype,
            Some(function) if function.is_async => self.async_prototype,
            Some(function) if function.is_generator => self.generator_prototype,
            _ => None,
        }
    }

    pub(crate) fn trace_roots(&self, visitor: &mut SlotVisitor<'_>) {
        for object in [
            self.generator_constructor,
            self.generator_prototype,
            self.async_constructor,
            self.async_prototype,
            self.async_generator_constructor,
            self.async_generator_prototype,
        ]
        .into_iter()
        .flatten()
        {
            object.trace_gc_roots(visitor);
        }
    }
}

impl Interpreter {
    pub(crate) fn install_function_kind_prototypes_post_bootstrap(&mut self) {
        let Some(function_proto) = self.function_prototype_object().ok() else {
            return;
        };
        let shape_root = self.shape_runtime.root();
        self.function_kind_prototypes = FunctionKindPrototypes::build_post_bootstrap(
            &mut self.gc_heap,
            shape_root,
            function_proto,
            &self.well_known_symbols,
        )
        .expect("function-kind prototypes fit within any positive cap");
    }

    pub(crate) fn function_kind_prototype_for(
        &self,
        context: &ExecutionContext,
        function_id: u32,
    ) -> Option<JsObject> {
        self.function_kind_prototypes
            .prototype_for(context, function_id)
    }

    /// Trace function-kind constructor/prototype caches.
    pub fn trace_function_kind_roots(&self, visitor: &mut SlotVisitor<'_>) {
        self.function_kind_prototypes.trace_roots(visitor);
    }
}
