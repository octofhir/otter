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

use crate::rooting::RootScopeExt;
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
        mut shape_root: object::ShapeHandle,
        function_proto: JsObject,
        function_ctor: Option<Value>,
        well_known: &crate::symbol::WellKnownSymbols,
    ) -> Result<Self, JsSurfaceError> {
        let mut function_proto_value = Value::object(function_proto);
        let mut function_ctor_value = function_ctor.unwrap_or_else(Value::undefined);
        let mut generator_ctor_root = Value::undefined();
        let mut generator_proto_root = Value::undefined();
        let mut async_ctor_root = Value::undefined();
        let mut async_proto_root = Value::undefined();
        let mut async_generator_ctor_root = Value::undefined();
        let mut async_generator_proto_root = Value::undefined();
        let mut roots = otter_gc::RootScope::new(heap);
        // SAFETY: every canonical slot is declared before the scope and stays
        // stationary until the completed cache is returned.
        unsafe {
            roots.add_raw_slot(
                (&mut shape_root as *mut object::ShapeHandle).cast::<otter_gc::raw::RawGc>(),
            );
            roots.add_value(&mut function_proto_value);
            roots.add_value(&mut function_ctor_value);
            roots.add_value(&mut generator_ctor_root);
            roots.add_value(&mut generator_proto_root);
            roots.add_value(&mut async_ctor_root);
            roots.add_value(&mut async_proto_root);
            roots.add_value(&mut async_generator_ctor_root);
            roots.add_value(&mut async_generator_proto_root);
        }
        let tag_sym = well_known.get(WellKnown::ToStringTag);
        let mut make = |tag: &'static str,
                        call: crate::native_function::NativeFastFn|
         -> Result<(JsObject, JsObject), JsSurfaceError> {
            let mut proto_root = Value::undefined();
            let mut ctor_root = Value::undefined();
            let mut native_root = Value::undefined();
            let mut name_root = Value::undefined();
            let mut local_roots = otter_gc::RootScope::new(heap);
            // SAFETY: all four slots precede the nested scope and remain live
            // through the complete constructor/prototype build.
            unsafe {
                local_roots.add_value(&mut proto_root);
                local_roots.add_value(&mut ctor_root);
                local_roots.add_value(&mut native_root);
                local_roots.add_value(&mut name_root);
            }
            proto_root = Value::object({
                let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
                    function_proto_value.trace_value_slots(visitor);
                };
                object::alloc_object_with_shape_roots(heap, shape_root, &mut visit)
                    .map_err(|_| JsSurfaceError::OutOfMemory)?
            });
            let proto = proto_root
                .as_object()
                .expect("function-kind prototype stays rooted");
            let function_proto = function_proto_value
                .as_object()
                .expect("Function.prototype stays rooted");
            object::set_prototype(proto, heap, Some(function_proto));
            let tag_sym_root = tag_sym;
            let tag_string = {
                let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
                    function_proto_value.trace_value_slots(visitor);
                    tag_sym_root.trace_value_slots(visitor);
                };
                JsString::from_str_with_roots(tag, heap, &mut visit)
                    .map_err(|_| JsSurfaceError::OutOfMemory)?
            };
            let proto = proto_root
                .as_object()
                .expect("function-kind prototype stays rooted after tag allocation");
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

            ctor_root = Value::object({
                let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
                    function_proto_value.trace_value_slots(visitor);
                    proto_root.trace_value_slots(visitor);
                };
                object::alloc_object_with_shape_roots(heap, shape_root, &mut visit)
                    .map_err(|_| JsSurfaceError::OutOfMemory)?
            });
            let mut ctor = ctor_root
                .as_object()
                .expect("function-kind constructor stays rooted");
            let mut proto = proto_root
                .as_object()
                .expect("function-kind prototype stays rooted");
            // §27.4.2 / §27.3.2 / §27.7.2 — the constructor's
            // [[Prototype]] is %Function% itself (these are Function
            // subclasses), falling back to %Function.prototype% in a
            // pre-bootstrap realm without a global Function binding.
            if function_ctor_value.is_undefined() {
                let function_proto = function_proto_value
                    .as_object()
                    .expect("Function.prototype stays rooted");
                object::set_prototype(ctor, heap, Some(function_proto));
            } else {
                object::set_prototype_value(ctor, heap, Some(function_ctor_value));
            }
            object::define_own_property_in_place(
                &mut ctor,
                heap,
                "prototype",
                object::PropertyDescriptor::data(proto_root, false, false, false),
            );
            ctor_root = Value::object(ctor);
            // §27.3.3.1 / §27.4.3.1 / §27.7.3.1 — { [[Writable]]:
            // false, [[Enumerable]]: false, [[Configurable]]: true }.
            object::define_own_property_in_place(
                &mut proto,
                heap,
                "constructor",
                object::PropertyDescriptor::data(ctor_root, false, false, true),
            );
            proto_root = Value::object(proto);
            native_root = Value::native_function({
                let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
                    function_proto_value.trace_value_slots(visitor);
                    ctor_root.trace_value_slots(visitor);
                    proto_root.trace_value_slots(visitor);
                };
                NativeFunction::new_constructor_static_with_roots(heap, tag, 1, call, &mut visit)
                    .map_err(|_| JsSurfaceError::OutOfMemory)?
            });
            ctor = ctor_root
                .as_object()
                .expect("function-kind constructor stays rooted");
            object::set_constructor_native(ctor, heap, native_root);
            // §27.3.2 / §27.4.2 / §27.7.2 — the constructor carries
            // `length = 1` and `name = tag`, both non-writable,
            // non-enumerable, configurable. Defined on the carrier
            // object since property reads resolve against it.
            name_root = Value::string({
                let mut visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
                    function_proto_value.trace_value_slots(visitor);
                    ctor_root.trace_value_slots(visitor);
                    proto_root.trace_value_slots(visitor);
                };
                JsString::from_str_with_roots(tag, heap, &mut visit)
                    .map_err(|_| JsSurfaceError::OutOfMemory)?
            });
            ctor = ctor_root
                .as_object()
                .expect("function-kind constructor stays rooted after name allocation");
            object::define_own_property_in_place(
                &mut ctor,
                heap,
                "length",
                object::PropertyDescriptor::data(Value::number_i32(1), false, false, true),
            );
            ctor_root = Value::object(ctor);
            object::define_own_property_in_place(
                &mut ctor,
                heap,
                "name",
                object::PropertyDescriptor::data(name_root, false, false, true),
            );
            ctor_root = Value::object(ctor);
            Ok((
                ctor_root
                    .as_object()
                    .expect("function-kind constructor stays rooted"),
                proto_root
                    .as_object()
                    .expect("function-kind prototype stays rooted"),
            ))
        };

        let (generator_constructor, generator_prototype) = make(
            "GeneratorFunction",
            crate::intrinsics::function::generator_function_ctor_call,
        )?;
        generator_ctor_root = Value::object(generator_constructor);
        generator_proto_root = Value::object(generator_prototype);
        let (async_constructor, async_prototype) = make(
            "AsyncFunction",
            crate::intrinsics::function::async_function_ctor_call,
        )?;
        async_ctor_root = Value::object(async_constructor);
        async_proto_root = Value::object(async_prototype);
        let (async_generator_constructor, async_generator_prototype) = make(
            "AsyncGeneratorFunction",
            crate::intrinsics::function::async_generator_function_ctor_call,
        )?;
        async_generator_ctor_root = Value::object(async_generator_constructor);
        async_generator_proto_root = Value::object(async_generator_prototype);
        Ok(Self {
            generator_constructor: generator_ctor_root.as_object(),
            generator_prototype: generator_proto_root.as_object(),
            async_constructor: async_ctor_root.as_object(),
            async_prototype: async_proto_root.as_object(),
            async_generator_constructor: async_generator_ctor_root.as_object(),
            async_generator_prototype: async_generator_proto_root.as_object(),
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
        let function_ctor = object::get(*self.global_this(), &self.gc_heap, "Function")
            .filter(|v| v.is_object_type());
        self.function_kind_prototypes = FunctionKindPrototypes::build_post_bootstrap(
            &mut self.gc_heap,
            shape_root,
            function_proto,
            function_ctor,
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
        use crate::intrinsics::iterator as iter_natives;
        let mut iterator_parent_root = self
            .constructor_prototype_value("Iterator")
            .ok()
            .unwrap_or_else(Value::undefined);
        let mut async_iterator_parent_root = Value::undefined();
        let mut sync_kind_root = self
            .function_kind_prototypes
            .generator_prototype
            .map(Value::object)
            .unwrap_or_else(Value::undefined);
        let mut async_kind_root = self
            .function_kind_prototypes
            .async_generator_prototype
            .map(Value::object)
            .unwrap_or_else(Value::undefined);
        let mut sync_shared_root = Value::undefined();
        let mut async_shared_root = Value::undefined();
        let mut roots = otter_gc::RootScope::new(&mut self.gc_heap);
        // SAFETY: all construction slots precede the scope and remain live
        // until the finished prototypes are installed in interpreter fields.
        unsafe {
            roots.add_value(&mut iterator_parent_root);
            roots.add_value(&mut async_iterator_parent_root);
            roots.add_value(&mut sync_kind_root);
            roots.add_value(&mut async_kind_root);
            roots.add_value(&mut sync_shared_root);
            roots.add_value(&mut async_shared_root);
        }
        // §27.1.3 %AsyncIteratorPrototype% — `[[Prototype]]` is
        // %Object.prototype%; carries `@@asyncIterator` returning the
        // receiver. %AsyncGeneratorPrototype% inherits from it.
        if let Some(proto) = self.build_async_iterator_prototype() {
            async_iterator_parent_root = Value::object(proto);
        }
        type Methods = [(&'static str, crate::native_function::NativeFastFn); 3];
        let sync_methods: Methods = [
            ("next", iter_natives::generator_proto_next),
            ("return", iter_natives::generator_proto_return),
            ("throw", iter_natives::generator_proto_throw),
        ];
        let async_methods: Methods = [
            ("next", iter_natives::async_generator_proto_next),
            ("return", iter_natives::async_generator_proto_return),
            ("throw", iter_natives::async_generator_proto_throw),
        ];
        let tag_sym = self.well_known_symbols.get(WellKnown::ToStringTag);
        for index in 0..2 {
            let (mut kind_value, mut parent_value, tag, methods) = if index == 0 {
                (
                    sync_kind_root,
                    iterator_parent_root,
                    "Generator",
                    sync_methods,
                )
            } else {
                (
                    async_kind_root,
                    async_iterator_parent_root,
                    "AsyncGenerator",
                    async_methods,
                )
            };
            if kind_value.as_object().is_none() {
                continue;
            }
            let mut shared_value = Value::undefined();
            let mut iteration_roots = otter_gc::RootScope::new(&mut self.gc_heap);
            // SAFETY: these per-iteration canonical slots precede the scope and
            // are never borrowed across a GC safepoint. The collector may
            // rewrite them in place; each use below reloads the current value.
            unsafe {
                iteration_roots.add_value(&mut kind_value);
                iteration_roots.add_value(&mut parent_value);
                iteration_roots.add_value(&mut shared_value);
            }
            let Ok(shared) = ({
                let mut visit = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
                object::alloc_object_with_shape_roots(
                    &mut self.gc_heap,
                    self.shape_runtime.root(),
                    &mut visit,
                )
            }) else {
                continue;
            };
            shared_value = Value::object(shared);
            if let Some(parent) = parent_value.as_object() {
                object::set_prototype(shared, &mut self.gc_heap, Some(parent));
            }
            // §27.5.1.2-5 / §27.6.1.2-4 — own `next` / `return` /
            // `throw`, each with `length = 1`, { [[Writable]]: true,
            // [[Enumerable]]: false, [[Configurable]]: true }.
            for (name, call) in methods {
                let Ok(native) = crate::intrinsics::shared::native_static_with_value_roots(
                    &mut self.gc_heap,
                    name,
                    1,
                    call,
                    &[],
                ) else {
                    continue;
                };
                let shared = shared_value
                    .as_object()
                    .expect("shared generator prototype stays rooted");
                object::define_own_property(
                    shared,
                    &mut self.gc_heap,
                    name,
                    object::PropertyDescriptor::data(
                        Value::native_function(native),
                        true,
                        false,
                        true,
                    ),
                );
            }
            let Ok(tag_string) = JsString::from_str(tag, &mut self.gc_heap) else {
                continue;
            };
            let shared = shared_value
                .as_object()
                .expect("shared generator prototype stays rooted");
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
                object::PropertyDescriptor::data(kind_value, false, false, true),
            );
            let kind_proto = kind_value
                .as_object()
                .expect("function-kind prototype stays rooted");
            object::define_own_property(
                kind_proto,
                &mut self.gc_heap,
                "prototype",
                object::PropertyDescriptor::data(shared_value, false, false, true),
            );
            if index == 0 {
                sync_shared_root = shared_value;
            } else {
                async_shared_root = shared_value;
            }
        }
        self.function_kind_prototypes.generator_object_prototype = sync_shared_root.as_object();
        self.function_kind_prototypes
            .async_generator_object_prototype = async_shared_root.as_object();
    }

    /// §27.1.3 — allocate `%AsyncIteratorPrototype%` with its
    /// `@@asyncIterator` self-returner.
    fn build_async_iterator_prototype(&mut self) -> Option<JsObject> {
        let mut object_proto_root = self
            .object_prototype_object_opt()
            .map(Value::object)
            .unwrap_or_else(Value::undefined);
        let mut proto_root = Value::undefined();
        let mut native_root = Value::undefined();
        let mut roots = otter_gc::RootScope::new(&mut self.gc_heap);
        // SAFETY: every slot precedes the scope and remains live until return.
        unsafe {
            roots.add_value(&mut object_proto_root);
            roots.add_value(&mut proto_root);
            roots.add_value(&mut native_root);
        }
        proto_root = Value::object({
            let mut visit = |_visitor: &mut dyn FnMut(*mut RawGc)| {};
            object::alloc_object_with_shape_roots(
                &mut self.gc_heap,
                self.shape_runtime.root(),
                &mut visit,
            )
            .ok()?
        });
        let proto = proto_root.as_object()?;
        if let Some(object_proto) = object_proto_root.as_object() {
            object::set_prototype(proto, &mut self.gc_heap, Some(object_proto));
        }
        native_root = Value::native_function(
            crate::intrinsics::shared::native_static_with_value_roots(
                &mut self.gc_heap,
                "[Symbol.asyncIterator]",
                0,
                crate::intrinsics::iterator::async_iterator_proto_symbol_async_iterator,
                &[&proto_root],
            )
            .ok()?,
        );
        let async_iter_sym = self.well_known_symbols.get(WellKnown::AsyncIterator);
        let proto = proto_root.as_object()?;
        object::define_own_symbol_property_partial(
            proto,
            &mut self.gc_heap,
            async_iter_sym,
            object::PartialPropertyDescriptor {
                value: Some(native_root),
                writable: Some(true),
                enumerable: Some(false),
                configurable: Some(true),
                ..Default::default()
            },
        );
        proto_root.as_object()
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
