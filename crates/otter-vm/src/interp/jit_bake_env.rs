//! What one compilation's bake reads of its isolate.
//!
//! # Contents
//! - [`BakeEnv`] — the isolate as baking sees it: the shared compile tables,
//!   the realm facts and scalars captured when the compilation was prepared,
//!   and the heap, registry and shape tables it still reads in place.
//! - The isolate queries baking makes, answered from those parts alone.
//!
//! # Invariants
//! - Baking reaches its isolate only through a `BakeEnv`. Every shared table
//!   is read through `MutatorOwned::try_read`; a table the mutator is writing
//!   reads as empty, which drops the facts it would have given.
//! - Captured realm facts are the ones current when the environment was
//!   captured; install-time dependency checks cover any later change.
//!
//! # See also
//! - [`crate::compile_shared`] — the shared tables.
//! - `jit_bake` — the reader.

use crate::compile_shared::CompileShared;
use crate::object::{JsObject, ShapeHandle, ShapeId};
use crate::property_atom::AtomizedPropertyKey;
use crate::{CodeBlock, Interpreter, MethodCallFeedback, jit, native_abi};

/// The isolate as one compilation's bake reads it.
pub(crate) struct BakeEnv<'a> {
    pub(crate) shared: &'a CompileShared,
    pub(crate) heap: &'a otter_gc::GcHeap,
    pub(crate) shapes: &'a crate::object::ShapeRuntime,
    pub(crate) string_prototype: Option<JsObject>,
    pub(crate) map_prototype: Option<JsObject>,
    pub(crate) set_prototype: Option<JsObject>,
    pub(crate) function_prototype: Option<JsObject>,
    /// The active realm's intrinsic callables, by call kind.
    pub(crate) callables: [crate::native_function::NativeFunction; crate::IntrinsicCallKind::COUNT],
    pub(crate) global_this: JsObject,
    pub(crate) active_realm_id: u32,
    pub(crate) object_prototype: Option<JsObject>,
    pub(crate) default_array_prototype: Option<JsObject>,
    pub(crate) null_prototype_root: ShapeHandle,
    pub(crate) stamp_prototypes: [Option<JsObject>; 5],
    pub(crate) array_iteration: Option<crate::iteration_protocol::ArrayIterationCells>,
    pub(crate) debug: crate::jit_debug::JitDebugRequest,
    pub(crate) fuel_cell: usize,
    pub(crate) property_action_cache: jit::JitPropertyActionCache,
    pub(crate) machine_allocation_allowed: bool,
    /// Functions retraining when the compilation was prepared.
    pub(crate) retraining: Vec<u32>,
}

impl<'a> BakeEnv<'a> {
    /// Capture `vm` as one compilation's bake reads it.
    pub(crate) fn capture(vm: &'a Interpreter) -> Self {
        Self {
            shared: &vm.compile_shared,
            heap: &vm.gc_heap,
            shapes: &vm.shape_runtime,
            string_prototype: vm.realm_intrinsics.string_prototype(),
            map_prototype: vm.realm_intrinsics.map_prototype(),
            set_prototype: vm.realm_intrinsics.set_prototype(),
            function_prototype: vm.realm_intrinsics.function_prototype(),
            callables: std::array::from_fn(|kind| {
                vm.realm_intrinsics
                    .callable(crate::IntrinsicCallKind::ALL[kind])
                    .unwrap_or_else(|| {
                        crate::native_function::NativeFunction::from_gc(otter_gc::Gc::null())
                    })
            }),
            global_this: vm.global_this,
            active_realm_id: vm.active_realm_id,
            object_prototype: vm.object_prototype_object_opt(),
            default_array_prototype: vm.default_array_prototype(),
            null_prototype_root: vm.null_prototype_root(),
            stamp_prototypes: vm.intrinsic_stamp_prototypes(),
            array_iteration: vm.proven_array_iteration_cells(),
            debug: vm.jit_debug.request(),
            fuel_cell: vm.interrupt.fuel_ptr() as usize,
            property_action_cache: vm.property_cache.jit_layout(),
            machine_allocation_allowed: vm.gc_heap.machine_allocation_allowed(),
            retraining: vm.optimizing_tier_policy.retraining_functions(),
        }
    }

    /// The active realm's intrinsic callable of `kind`.
    pub(crate) fn callable(
        &self,
        kind: crate::IntrinsicCallKind,
    ) -> Option<crate::native_function::NativeFunction> {
        let callable = self.callables[kind as usize];
        (!callable.raw().is_null()).then_some(callable)
    }

    /// Whether `fid` stays below generated code to collect fresh feedback.
    pub(crate) fn jit_retraining_blocks(&self, fid: u32) -> bool {
        self.retraining.contains(&fid)
    }

    /// The realm a bytecode function was linked in.
    pub(crate) fn function_realm_id(&self, function_id: u32) -> u32 {
        self.shared
            .function_realm_ids
            .try_read()
            .and_then(|realms| realms.get(&function_id).copied())
            .unwrap_or(0)
    }

    /// The realm of `function_id` when it is not the active one.
    pub(crate) fn foreign_function_realm(&self, function_id: u32) -> Option<u32> {
        let realm_id = self.function_realm_id(function_id);
        (realm_id != self.active_realm_id).then_some(realm_id)
    }

    /// The method target distribution of one method site.
    pub(crate) fn method_target_feedback(&self, site: usize) -> Option<MethodCallFeedback> {
        self.shared.method_feedback.try_read()?.method_targets(site)
    }

    /// The method-resolution IC of one method site.
    pub(crate) fn method_ic(&self, site: usize) -> Option<crate::method_ops::MethodCallIc> {
        self.shared.method_feedback.try_read()?.method_ic(site)
    }

    /// The optimizing exit evidence recorded for sites of `fid`.
    pub(crate) fn exit_profiles_of(
        &self,
        fid: u32,
    ) -> Vec<((u32, u32, native_abi::ExitReason), jit::JitExitProfile)> {
        let Some(profiles) = self.shared.jit_optimized_exit_profiles.try_read() else {
            return Vec::new();
        };
        profiles
            .range((fid, 0, native_abi::ExitReason::TypeMismatch)..)
            .take_while(|((profile_fid, _, _), _)| *profile_fid == fid)
            .map(|(key, profile)| (*key, *profile))
            .collect()
    }

    /// The parameter widening failed optimized entries of `fid` learned.
    pub(crate) fn parameter_widening(&self, fid: u32) -> Option<Box<[jit::JitParameterWidening]>> {
        self.shared
            .jit_parameter_widening
            .try_read()?
            .get(&fid)
            .cloned()
    }

    /// The resolved site of a regex literal, keyed like the literal cells.
    pub(crate) fn regexp_literal(
        &self,
        key: (usize, u32),
    ) -> Option<crate::regexp::RegExpLiteralSite> {
        self.shared.regexp_literals.try_read()?.get(&key).copied()
    }

    /// The hidden class of an object-literal site of `fid` whose first key
    /// constant is `first_key`.
    pub(crate) fn object_literal_layout(
        &self,
        fid: u32,
        first_key: u32,
    ) -> Option<crate::handles::ObjectLayout> {
        self.shared
            .object_literal_layouts
            .try_read()?
            .get(&(fid, first_key))
            .copied()
    }

    /// The shared slab of an array-literal boilerplate constant.
    pub(crate) fn array_boilerplate(
        &self,
        key: (usize, u32),
    ) -> Option<crate::array::elements::ElementSlabHandle> {
        self.shared
            .array_boilerplates
            .try_read()?
            .get(&key)
            .copied()
    }

    /// The global declarative binding `name`: its cell and whether it is
    /// constant.
    pub(crate) fn global_lexical(&self, name: &str) -> Option<(crate::UpvalueCell, bool)> {
        self.shared.global_lexicals.try_read()?.get(name).copied()
    }

    /// The global declarative record's name-set identity.
    pub(crate) fn global_lexical_epoch(&self) -> u64 {
        self.shared
            .global_lexical_epoch
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// How many fields the simple-constructor initializer of `fid` stores.
    pub(crate) fn simple_constructor_field_count(&self, fid: u32) -> Option<usize> {
        self.shared
            .simple_constructor_init_cache
            .try_read()?
            .get(&fid)?
            .as_ref()
            .map(|init| init.fields.len())
    }

    /// The final hidden class the simple constructor `fid` reaches from the
    /// root `root`.
    pub(crate) fn simple_constructor_shape(&self, fid: u32, root: ShapeId) -> Option<ShapeHandle> {
        self.shared
            .simple_constructor_shape_cache
            .try_read()?
            .get(&(fid, root))
            .copied()
    }

    /// Every final hidden class the simple constructor `fid` reaches.
    pub(crate) fn simple_constructor_shapes(&self, fid: u32) -> Vec<ShapeHandle> {
        let Some(shapes) = self.shared.simple_constructor_shape_cache.try_read() else {
            return Vec::new();
        };
        shapes
            .iter()
            .filter(|((id, _), _)| *id == fid)
            .map(|(_, shape)| *shape)
            .collect()
    }

    /// The stable entry cell plan of one compiler-native call.
    pub(crate) fn current_direct_callee_plan(
        &self,
        function: &CodeBlock,
    ) -> Option<jit::JitDirectCallPlan> {
        let address = self
            .shared
            .function_entries
            .try_read()?
            .get(&function.id)
            .copied()?;
        // SAFETY: directory addresses name registry-lifetime entry cells.
        crate::jit_registry::published_plan(
            unsafe { &*(address as *const crate::native_abi::FunctionEntryCell) },
            function,
        )
    }

    /// The classified context cell of `function_id`'s closures, or `0`.
    pub(crate) fn classified_closure_context(&self, function_id: u32) -> u64 {
        crate::closure_contexts::classified_of_cell(self.closure_context_cell(function_id))
    }

    /// The published context fact cell of `function_id`'s closures, or `0`.
    fn closure_context_cell(&self, function_id: u32) -> usize {
        self.shared
            .closure_context_cells
            .try_read()
            .and_then(|cells| cells.get(&function_id).copied())
            .unwrap_or(0)
    }

    /// The one context every closure of `function_id` was created with.
    pub(crate) fn single_closure_context(&self, function_id: u32) -> Option<(u64, crate::Value)> {
        let cell = self.closure_context_cell(function_id);
        crate::closure_contexts::single_of_cell(cell).map(|context| (cell as u64, context))
    }

    /// The watch slot `slot` of `context` would be watched under.
    pub(crate) fn watchable_context_slot(
        &self,
        context: crate::context::ContextHandle,
        slot: u16,
    ) -> Option<u32> {
        crate::code_dependencies::watchable_slot(self.heap, context, slot)
    }

    /// The hidden class with stable id `id`, while interned.
    pub(crate) fn shape_for_id(&self, id: ShapeId) -> Option<ShapeHandle> {
        self.shapes.handle_for_id(self.heap, id)
    }

    /// The slot offset `shape` gives the interned spelling `key`.
    pub(crate) fn shape_offset_of(&self, shape: ShapeHandle, key: &str) -> Option<u32> {
        let atom = crate::isolate_names::names(self.heap).lookup(key);
        if atom == crate::property_atom::AtomId::NONE {
            return None;
        }
        crate::object::shape_body::shape_slot_of_atom(self.heap, shape, atom)
            .map(|slot| slot.offset)
    }

    /// The live layout of a method holder: its hidden class, else its
    /// dictionary slot-layout epoch.
    pub(crate) fn method_holder_layout(
        &self,
        holder: JsObject,
    ) -> Option<super::jit_bake::MethodHolderLayout> {
        super::jit_bake::method_holder_layout(self.heap, self.string_prototype, holder)
    }

    /// The prepared intrinsic-prototype programs of `key` and the shapes
    /// they guard, while the layout they were built against still holds.
    pub(crate) fn prepared_intrinsic_property_programs(
        &self,
        key: AtomizedPropertyKey<'_>,
    ) -> Option<(Vec<jit::JitCacheIrProgram>, [ShapeHandle; 5])> {
        let (stamp, shapes) = super::jit_intrinsic_properties::layout_stamp(
            self.heap,
            self.stamp_prototypes,
            self.string_prototype,
            self.active_realm_id,
        );
        let memo = self.shared.jit_intrinsic_programs.try_read()?;
        let (built, programs) = memo.get(&key.atom().id().raw())?;
        (*built == stamp).then(|| (programs.clone(), shapes))
    }

    /// The finalized family `family_id` of base constructor `base`, with its
    /// new.target owner function and whether that owner is a class.
    pub(crate) fn finalized_constructor_layout_for_id(
        &self,
        family_id: u64,
        base: u32,
    ) -> Option<(crate::constructor_layout::ConstructorLayout, u32, bool)> {
        crate::constructor_layout::compile::finalized_layout(
            self.heap,
            self.shared,
            family_id,
            base,
        )
    }

    /// The site one generation bakes for a literal instruction's cell: the
    /// one its first allocation registered, else none.
    pub(crate) fn registered_jit_allocation_site(
        &self,
        cell: Option<&std::sync::atomic::AtomicU16>,
    ) -> jit::JitAllocationSite {
        crate::literal_allocation::registered_site(self.heap, cell)
    }
}
