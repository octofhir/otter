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
    js_surface::JsSurfaceError, native_function::NativeFunction, object, symbol::WellKnown,
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
    /// `%GeneratorPrototype%` (§27.5.1) — the shared prototype of
    /// every generator function's `.prototype` object.
    generator_object_prototype: Option<JsObject>,
    /// `%AsyncGeneratorPrototype%` (§27.6.1).
    async_generator_object_prototype: Option<JsObject>,
}

impl FunctionKindPrototypes {
    pub(crate) fn build_post_bootstrap(
        heap: &mut otter_gc::GcHeap,
        shape_root: object::ShapeHandle,
        function_proto: JsObject,
        well_known: &crate::symbol::WellKnownSymbols,
    ) -> Result<Self, JsSurfaceError> {
        let function_proto_value = Value::object(function_proto);
        let tag_sym = well_known.get(WellKnown::ToStringTag);
        let mut make = |tag: &'static str,
                        call: crate::native_function::NativeFastFn|
         -> Result<(JsObject, JsObject), JsSurfaceError> {
            let mut proto = {
                let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
                    function_proto_value.trace_value_slots(visitor);
                };
                object::alloc_object_with_shape_roots(heap, shape_root, &mut visit)
                    .map_err(|_| JsSurfaceError::OutOfMemory)?
            };
            object::set_prototype(proto, heap, Some(function_proto));
            let tag_sym_root = tag_sym;
            let tag_string = {
                let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
                    function_proto_value.trace_value_slots(visitor);
                    tag_sym_root.trace_value_slots(visitor);
                    let p = &mut proto as *mut JsObject as *mut RawGc;
                    visitor(p);
                };
                JsString::from_str_with_roots(tag, heap, &mut visit)
                    .map_err(|_| JsSurfaceError::OutOfMemory)?
            };
            object::define_own_symbol_property_partial(
                proto,
                heap,
                tag_sym_root,
                object::PartialPropertyDescriptor {
                    value: Some(Value::string(tag_string)),
                    writable: Some(false),
                    enumerable: Some(false),
                    configurable: Some(true),
                    ..Default::default()
                },
            );

            let proto_value = Value::object(proto);
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
            // §27.3.3.1 / §27.4.3.1 / §27.7.3.1 — { [[Writable]]:
            // false, [[Enumerable]]: false, [[Configurable]]: true }.
            object::define_own_property(
                proto,
                heap,
                "constructor",
                object::PropertyDescriptor::data(Value::object(ctor), false, false, true),
            );
            let ctor_root = Value::object(ctor);
            let proto_root = Value::object(proto);
            let native = {
                let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
                    function_proto_value.trace_value_slots(visitor);
                    ctor_root.trace_value_slots(visitor);
                    proto_root.trace_value_slots(visitor);
                };
                NativeFunction::new_constructor_static_with_roots(heap, tag, 1, call, &mut visit)
                    .map_err(|_| JsSurfaceError::OutOfMemory)?
            };
            object::set_constructor_native(ctor, heap, Value::native_function(native));
            // §27.3.2 / §27.4.2 / §27.7.2 — the constructor carries
            // `length = 1` and `name = tag`, both non-writable,
            // non-enumerable, configurable. Defined on the carrier
            // object since property reads resolve against it.
            let name_string = {
                let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
                    function_proto_value.trace_value_slots(visitor);
                    ctor_root.trace_value_slots(visitor);
                    proto_root.trace_value_slots(visitor);
                };
                JsString::from_str_with_roots(tag, heap, &mut visit)
                    .map_err(|_| JsSurfaceError::OutOfMemory)?
            };
            object::define_own_property(
                ctor,
                heap,
                "length",
                object::PropertyDescriptor::data(Value::number_i32(1), false, false, true),
            );
            object::define_own_property(
                ctor,
                heap,
                "name",
                object::PropertyDescriptor::data(Value::string(name_string), false, false, true),
            );
            Ok((ctor, proto))
        };

        let (generator_constructor, generator_prototype) = make(
            "GeneratorFunction",
            crate::intrinsics::function::generator_function_ctor_call,
        )?;
        let (async_constructor, async_prototype) = make(
            "AsyncFunction",
            crate::intrinsics::function::async_function_ctor_call,
        )?;
        let (async_generator_constructor, async_generator_prototype) = make(
            "AsyncGeneratorFunction",
            crate::intrinsics::function::async_generator_function_ctor_call,
        )?;
        Ok(Self {
            generator_constructor: Some(generator_constructor),
            generator_prototype: Some(generator_prototype),
            async_constructor: Some(async_constructor),
            async_prototype: Some(async_prototype),
            async_generator_constructor: Some(async_generator_constructor),
            async_generator_prototype: Some(async_generator_prototype),
            generator_object_prototype: None,
            async_generator_object_prototype: None,
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

    /// Flag-based variant of [`Self::prototype_for`] for callers
    /// resolving function metadata through the code space rather than
    /// an [`ExecutionContext`].
    pub(crate) fn kind_prototype_for_flags(
        &self,
        is_generator: bool,
        is_async: bool,
    ) -> Option<JsObject> {
        match (is_generator, is_async) {
            (true, true) => self.async_generator_prototype,
            (true, false) => self.generator_prototype,
            (false, true) => self.async_prototype,
            (false, false) => None,
        }
    }

    pub(crate) fn trace_roots(&self, visitor: &mut SlotVisitor<'_>) {
        for object in [
            &self.generator_constructor,
            &self.generator_prototype,
            &self.async_constructor,
            &self.async_prototype,
            &self.async_generator_constructor,
            &self.async_generator_prototype,
            &self.generator_object_prototype,
            &self.async_generator_object_prototype,
        ]
        .into_iter()
        .filter_map(Option::as_ref)
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
        self.install_shared_generator_object_prototypes();
    }

    /// §27.5.1 `%GeneratorPrototype%` / §27.6.1 `%AsyncGeneratorPrototype%`:
    /// one shared object per kind, wired as
    /// `%GeneratorFunction.prototype%.prototype` (writable: false,
    /// enumerable: false, configurable: true) with a back-pointing
    /// `constructor` and the kind's `@@toStringTag`. Every generator
    /// function's own `.prototype` object inherits from it.
    fn install_shared_generator_object_prototypes(&mut self) {
        let iterator_proto = self
            .constructor_prototype_value("Iterator")
            .ok()
            .and_then(|v| v.as_object());
        let pairs = [
            (
                self.function_kind_prototypes.generator_prototype,
                "Generator",
            ),
            (
                self.function_kind_prototypes.async_generator_prototype,
                "AsyncGenerator",
            ),
        ];
        let tag_sym = self.well_known_symbols.get(WellKnown::ToStringTag);
        let mut built: [Option<JsObject>; 2] = [None, None];
        for (slot, (kind_proto, tag)) in built.iter_mut().zip(pairs) {
            let Some(kind_proto) = kind_proto else {
                continue;
            };
            let kind_proto_value = Value::object(kind_proto);
            let Ok(shared) = ({
                let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
                    kind_proto_value.trace_value_slots(visitor);
                };
                object::alloc_object_with_shape_roots(
                    &mut self.gc_heap,
                    self.shape_runtime.root(),
                    &mut visit,
                )
            }) else {
                continue;
            };
            if let Some(iterator_proto) = iterator_proto {
                object::set_prototype(shared, &mut self.gc_heap, Some(iterator_proto));
            }
            let Ok(tag_string) = JsString::from_str(tag, &mut self.gc_heap) else {
                continue;
            };
            object::define_own_symbol_property_partial(
                shared,
                &mut self.gc_heap,
                tag_sym,
                object::PartialPropertyDescriptor {
                    value: Some(Value::string(tag_string)),
                    writable: Some(false),
                    enumerable: Some(false),
                    configurable: Some(true),
                    ..Default::default()
                },
            );
            object::define_own_property(
                shared,
                &mut self.gc_heap,
                "constructor",
                object::PropertyDescriptor::data(kind_proto_value, false, false, true),
            );
            object::define_own_property(
                kind_proto,
                &mut self.gc_heap,
                "prototype",
                object::PropertyDescriptor::data(Value::object(shared), false, false, true),
            );
            *slot = Some(shared);
        }
        self.function_kind_prototypes.generator_object_prototype = built[0];
        self.function_kind_prototypes
            .async_generator_object_prototype = built[1];
    }

    /// Shared `%GeneratorPrototype%` / `%AsyncGeneratorPrototype%` for
    /// a generator function's `.prototype` parent.
    pub(crate) fn shared_generator_object_prototype(&self, is_async: bool) -> Option<JsObject> {
        if is_async {
            self.function_kind_prototypes
                .async_generator_object_prototype
        } else {
            self.function_kind_prototypes.generator_object_prototype
        }
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
