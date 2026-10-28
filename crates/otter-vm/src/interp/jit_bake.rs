//! Compile-input baking: profile feedback and heap facts frozen into one
//! [`jit::JitCompileSnapshot`] for the template and optimizing compilers.
//!
//! # Contents
//! - [`Baker`] — one compilation's baking pass. It reads the isolate through a
//!   shared reference only, and owns everything the compilation adds: the
//!   roots its snapshot names, the install tasks the isolate runs before the
//!   code is published, and the debug events it records.
//! - Per-site passes (literals, globals, property CacheIR, element and keyed
//!   accesses, guarded methods, closures, exit evidence) and the inline-body
//!   tree (call-site plans, method targets, field types, proved stores).
//!
//! # Invariants
//! - Baking has no effect the mutator can observe: it allocates nothing on the
//!   GC heap, creates or registers no hidden class, subscribes no watchpoint
//!   and grows no isolate cache. Facts a site needs are prepared where they
//!   arise (first execution, an IC miss, template compilation); a fact that
//!   does not exist yet drops that one specialization.
//! - Because baking cannot collect, every identity it reads stays valid until
//!   [`Baker::finish`] hands the roots to the pending compilation, which the
//!   collector traces from then on.
//! - Requests that must change mutator state (context-slot and dictionary
//!   watches, `instanceof` cell registration) are recorded as install tasks
//!   and run on the isolate thread, which re-verifies each one before the
//!   code depending on it is published.
//!
//! # See also
//! - `jit_compile` — compile sessions, queueing and installation.
//! - [`crate::jit_roots`] — what a compilation's roots keep alive.
#![allow(unused_imports)]
use crate::*;

#[path = "inline_snapshot_budget.rs"]
mod inline_snapshot_budget;
use inline_snapshot_budget::InlineSnapshotBudget;

#[path = "jit_field_types.rs"]
mod field_types;

#[path = "jit_inline_methods.rs"]
mod inline_methods;

#[path = "jit_proved_stores.rs"]
mod proved_stores;

#[path = "jit_receiver_methods.rs"]
mod receiver_methods;
use receiver_methods::ConstructReceiver;

#[cfg(test)]
#[path = "jit_bake_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "jit_compile_binding_tests.rs"]
mod binding_tests;

#[cfg(test)]
#[path = "jit_method_targets_tests.rs"]
mod method_target_tests;

#[cfg(test)]
#[path = "jit_compile_group_tests.rs"]
mod group_policy_tests;

/// Whether a literal cell holds the primitive `op` loads.
pub(super) fn literal_matches(op: Op, value: &Value) -> bool {
    if op == Op::LoadString {
        value.is_string()
    } else {
        value.is_big_int()
    }
}

/// The hidden class a shape guard names: `None` for a null, dictionary or
/// opaque shape, which no native proof may name.
pub(crate) fn bakeable_shape(shape: crate::object::ShapeHandle) -> Option<u32> {
    if shape.is_null() {
        return None;
    }
    let state = crate::object::shape_body::state_of(shape);
    (!state.is_dictionary() && !state.is_opaque()).then(|| shape.offset())
}

/// How a method holder's slot layout is named: by its hidden class, or by its
/// dictionary slot-layout epoch.
#[derive(Clone, Copy)]
pub(crate) enum MethodHolderLayout {
    Shape(crate::object::ShapeHandle),
    Dictionary(u64),
}

impl Interpreter {
    /// The live layout of a method holder: its hidden class, else its
    /// dictionary slot-layout epoch. Reads only.
    pub(crate) fn method_holder_layout(
        &self,
        holder: crate::object::JsObject,
    ) -> Option<MethodHolderLayout> {
        let shape = crate::object::keyed_shape(holder, &self.gc_heap);
        if !shape.is_null() {
            return Some(MethodHolderLayout::Shape(shape));
        }
        let state = crate::object::state(holder, &self.gc_heap);
        if !state.is_dictionary() {
            return None;
        }
        if state.is_opaque()
            && (Some(holder) != self.realm_intrinsics.string_prototype()
                || state.bits() & crate::object::ShapeState::OPAQUE_LOOKUP_MASK
                    != crate::object::ShapeState::STRING_WRAPPER_MASK)
        {
            return None;
        }
        // The only opaque dictionary holder admitted here is the pinned
        // source-realm String prototype. Its callers prove primitive receiver
        // type plus nonvirtual names (fixed builtin names or explicit index/
        // length exclusion), then own Data and watched layout/live callable.
        crate::object::dictionary_layout(holder, &self.gc_heap)
            .map(|layout| MethodHolderLayout::Dictionary(u64::from(layout)))
    }
}

/// One compilation's baking pass over the isolate's feedback and heap.
pub(crate) struct Baker<'a> {
    pub(crate) vm: &'a Interpreter,
    /// Every allocation the snapshot being baked names.
    roots: Vec<crate::jit_roots::CompilationRoot>,
    /// Mutator-side work the isolate runs before publishing the code.
    install: InstallTasks,
    /// Debug events recorded while baking, published by the session.
    events: Vec<jit_debug::JitDebugEvent>,
    /// Hidden classes this pass found itself (a family's initial shape, a
    /// method holder's root) by id, for the steps that name them by id. A
    /// handle stays valid for the whole pass: baking cannot collect.
    found_shapes: rustc_hash::FxHashMap<crate::object::ShapeId, crate::object::ShapeHandle>,
    /// Marks the heap as read by this pass while it lives.
    _scope: otter_gc::CompilationScope<'a>,
}

impl<'a> Baker<'a> {
    pub(crate) fn new(vm: &'a Interpreter) -> Self {
        Self {
            vm,
            roots: Vec::new(),
            install: InstallTasks::default(),
            events: Vec::new(),
            found_shapes: rustc_hash::FxHashMap::default(),
            _scope: vm.gc_heap.enter_compilation_scope(),
        }
    }

    /// The hidden class with stable id `id`: the isolate's interned one, else
    /// one this pass found; `None` once collected or never named.
    pub(crate) fn shape_for_id(
        &mut self,
        id: crate::object::ShapeId,
    ) -> Option<crate::object::ShapeHandle> {
        self.vm
            .shape_runtime
            .handle_for_id(&self.vm.gc_heap, id)
            .or_else(|| self.found_shapes.get(&id).copied())
    }

    /// Name `shape` by its id for the rest of this pass.
    fn found_shape(&mut self, shape: crate::object::ShapeHandle) -> crate::object::ShapeId {
        let id = crate::object::shape_body::id_of(shape);
        self.found_shapes.insert(id, shape);
        id
    }

    /// Record a debug event when the isolate captures them.
    fn record_event(&mut self, event: impl FnOnce() -> jit_debug::JitDebugEvent) {
        if self.vm.jit_debug.request().events_enabled() {
            self.events.push(event());
        }
    }

    pub(super) fn record_jit_inline_candidate(
        &mut self,
        caller_function_id: u32,
        instruction_pc: u32,
        tier: jit_debug::JitDebugTier,
        callee_function_id: Option<u32>,
        bake_rejection: Option<jit_debug::JitInlineRejectionReason>,
    ) {
        self.record_event(|| jit_debug::JitDebugEvent::InlineCandidate {
            caller_function_id,
            instruction_pc,
            tier,
            callee_function_id,
            bake_rejection,
        });
    }

    pub(super) fn record_jit_direct_call_plan(
        &mut self,
        call_kind: jit::JitDirectCallKind,
        caller_function_id: u32,
        instruction_pc: u32,
        tier: jit_debug::JitDebugTier,
        callee_function_id: u32,
        target_index: u32,
        target_count: u32,
        outcome: jit_debug::JitDirectCallPlanOutcome,
    ) {
        self.record_event(|| jit_debug::JitDebugEvent::DirectCallPlan {
            call_kind,
            caller_function_id,
            instruction_pc,
            tier,
            callee_function_id,
            target_index,
            target_count,
            outcome,
        });
    }

    pub(super) fn record_jit_static_native_call_plan(
        &mut self,
        caller_function_id: u32,
        instruction_pc: u32,
        tier: jit_debug::JitDebugTier,
        target: &'static str,
    ) {
        self.record_event(|| jit_debug::JitDebugEvent::StaticNativeCallPlan {
            caller_function_id,
            instruction_pc,
            tier,
            target,
        });
    }

    /// The roots the baked snapshot names, sorted and without duplicates, the
    /// install tasks, and the recorded events.
    pub(crate) fn finish(
        self,
    ) -> (
        Box<[crate::jit_roots::CompilationRoot]>,
        InstallTasks,
        Vec<jit_debug::JitDebugEvent>,
    ) {
        let mut roots = self.roots;
        roots.sort_unstable_by_key(crate::jit_roots::CompilationRoot::key);
        roots.dedup_by_key(|root| root.key());
        (roots.into_boxed_slice(), self.install, self.events)
    }
}

impl Baker<'_> {
    /// Bake the template tier's compile input for `fid`, whose facts the
    /// session prepared.
    pub(crate) fn bake_template(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
    ) -> Option<jit::JitCompileSnapshot> {
        let mut view = context.jit_compile_snapshot(fid)?;
        // Realm guards compare the active realm with the function's own.
        view.literal_allocations.realm_id = self.vm.function_realm_id(fid);
        Self::bake_typed_array_layout(&mut view);
        Self::bake_string_layout(&mut view);
        self.bake_literal_cells(&mut view, context, fid, true)?;
        self.bake_global_lexical_loads(&mut view, context, fid);
        self.bake_binding_hit_proofs(&mut view, context, fid);
        self.bake_property_cache_ir(&mut view, context);
        self.bake_inline_callees(&mut view, context, fid, jit_debug::JitDebugTier::Template);
        self.bake_guarded_method_calls(&mut view, context, fid);
        self.bake_forward_apply(&mut view);
        self.bake_element_accesses(&mut view);
        self.bake_keyed_name_sites(&mut view);
        Self::bake_context_allocations(&mut view);
        self.bake_closure_allocations(&mut view, context);
        self.bake_field_type_tree(&mut view);
        Some(view)
    }

    /// Bake the optimizing tier's compile input for `fid` and every body it
    /// may splice.
    pub(crate) fn bake_optimized(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
        dependency_flag: std::sync::Arc<crate::code_dependencies::DependencyFlag>,
    ) -> Option<jit::JitCompileSnapshot> {
        let mut snapshot = context.jit_compile_snapshot(fid)?;
        snapshot.dependency_flag = dependency_flag;
        self.bake_literal_allocations(&mut snapshot, context, fid)?;
        // The optimizing tier consumes the same baked compile inputs as the
        // template tier: without the cage base and body offsets no inline access
        // can be emitted at all, and without monomorphic call-site candidates
        // there is nothing to inline.
        Self::bake_typed_array_layout(&mut snapshot);
        Self::bake_string_layout(&mut snapshot);
        self.bake_literal_cells(&mut snapshot, context, fid, false);
        self.bake_global_lexical_loads(&mut snapshot, context, fid);
        self.bake_binding_hit_proofs(&mut snapshot, context, fid);
        self.bake_property_cache_ir(&mut snapshot, context);
        self.bake_inline_callees(
            &mut snapshot,
            context,
            fid,
            jit_debug::JitDebugTier::Optimizing,
        );
        self.bake_unique_closure_context(&mut snapshot);
        self.bake_guarded_method_calls(&mut snapshot, context, fid);
        self.bake_instanceof_cells(&mut snapshot);
        self.bake_forward_apply(&mut snapshot);
        self.bake_element_accesses(&mut snapshot);
        self.bake_keyed_name_sites(&mut snapshot);
        Self::bake_context_allocations(&mut snapshot);
        self.bake_closure_allocations(&mut snapshot, context);
        self.bake_optimized_exit_profile(&mut snapshot, fid);
        self.bake_own_receiver_plan(&mut snapshot, fid);
        self.bake_proved_stores_tree(&mut snapshot);
        self.bake_field_type_tree(&mut snapshot);
        // Every shape baked so far that no object has left yet: a value
        // proven to have one survives calls while the generation's
        // subscription to the shape's stability holds. The rest have been
        // left already, and stay so.
        for root in &self.roots {
            if let crate::jit_roots::CompilationRoot::Shape(shape) = root {
                if crate::object::shape_body::is_stable(*shape) {
                    snapshot.stable_shapes.insert(shape.offset());
                } else {
                    snapshot.left_shapes.insert(shape.offset());
                }
            }
        }
        Some(snapshot)
    }

    /// Name `shape` in the code being compiled and return the compressed
    /// handle generated code compares or publishes. Hidden classes are
    /// collectable, so the compilation keeps the shape alive and the
    /// installed code object takes it over ([`Self::finalized_compile_roots`]).
    /// Unsupported state declines only this native proof, never publishes a
    /// fabricated zero identity or permanently poisons dynamic IC feedback.
    pub(crate) fn bake_shape(&mut self, shape: crate::object::ShapeHandle) -> Option<u32> {
        let offset = bakeable_shape(shape)?;
        self.roots
            .push(crate::jit_roots::CompilationRoot::Shape(shape));
        Some(offset)
    }

    /// A fresh callee identity cell for one call site of the generation being
    /// compiled; the installed code object retains it.
    pub(crate) fn bake_callee_identity_cell(&mut self) -> u64 {
        let cell = std::sync::Arc::new(crate::jit_roots::CalleeIdentityCell::new());
        let address = cell.address();
        self.roots
            .push(crate::jit_roots::CompilationRoot::CalleeIdentity(cell));
        address
    }

    /// An identity cell that holds `value` for generated code to compare,
    /// retained by the compilation and rewritten by the collector.
    pub(crate) fn bake_held_identity_cell(&mut self, value: Value) -> u64 {
        self.held_cell(value).address()
    }

    /// [`Self::bake_held_identity_cell`]'s cell itself.
    fn held_cell(&mut self, value: Value) -> std::sync::Arc<crate::jit_roots::CalleeIdentityCell> {
        let cell = std::sync::Arc::new(crate::jit_roots::CalleeIdentityCell::holding(value));
        self.roots
            .push(crate::jit_roots::CompilationRoot::CalleeIdentity(
                std::sync::Arc::clone(&cell),
            ));
        cell
    }

    /// A fresh `instanceof` cell for one generated site, retained by the
    /// compilation and registered at install for
    /// [`Interpreter::retire_instanceof_proofs_for`].
    pub(crate) fn bake_instanceof_cell(&mut self) -> u64 {
        let cell = std::sync::Arc::new(crate::jit_roots::InstanceofCell::new());
        let address = cell.address();
        self.install
            .instanceof_cells
            .push(std::sync::Arc::clone(&cell));
        self.roots
            .push(crate::jit_roots::CompilationRoot::Instanceof(cell));
        address
    }

    /// Bake one `instanceof` cell for every `instanceof` site of `view`.
    fn bake_instanceof_cells(&mut self, view: &mut jit::JitCompileSnapshot) {
        let sites: Vec<u32> = view
            .instructions
            .iter()
            .filter(|instruction| instruction.op(&view.code_block) == Op::Instanceof)
            .map(|instruction| instruction.byte_pc)
            .collect();
        for byte_pc in sites {
            view.instanceof_cells
                .insert(byte_pc, self.bake_instanceof_cell());
        }
    }

    /// Retain a valid chain proof for the generation being compiled.
    pub(crate) fn bake_prototype_validity(
        &mut self,
        cell: &std::sync::Arc<crate::object::prototype_validity::PrototypeValidity>,
    ) -> Option<jit::JitPrototypeValidity> {
        if !cell.is_valid() {
            return None;
        }
        self.roots
            .push(crate::jit_roots::CompilationRoot::Prototype(cell.clone()));
        Some(jit::JitPrototypeValidity {
            address: cell.address(),
            identity: cell.identity(),
        })
    }

    /// [`Self::bake_shape`] for a shape feedback names by id; `None` when
    /// that shape has been collected since or cannot authorize a native proof.
    pub(crate) fn bake_shape_id(&mut self, id: crate::object::ShapeId) -> Option<u32> {
        self.shape_for_id(id)
            .and_then(|shape| self.bake_shape(shape))
    }

    /// The live layout of a method holder as a generated guard names it: its
    /// baked hidden class, else its dictionary slot-layout epoch.
    pub(crate) fn jit_method_holder(
        &mut self,
        holder: crate::object::JsObject,
    ) -> Option<jit::JitMethodHolder> {
        match self.vm.method_holder_layout(holder)? {
            MethodHolderLayout::Shape(shape) => {
                self.bake_shape(shape).map(jit::JitMethodHolder::Shape)
            }
            MethodHolderLayout::Dictionary(layout) => {
                Some(jit::JitMethodHolder::Dictionary(layout))
            }
        }
    }

    /// The prepared intrinsic-prototype programs of `key`, with the shapes
    /// they guard rooted for this compilation; none when the session never
    /// prepared them for the current layout.
    fn intrinsic_property_programs(
        &mut self,
        key: crate::property_atom::AtomizedPropertyKey<'_>,
    ) -> Vec<jit::JitCacheIrProgram> {
        let vm = self.vm;
        let Some((programs, shapes)) = vm.prepared_intrinsic_property_programs(key) else {
            return Vec::new();
        };
        for shape in shapes {
            let _ = self.bake_shape(shape);
        }
        programs.to_vec()
    }

    /// Prepare exact source-realm literal shapes and scalar dense geometry.
    fn bake_literal_allocations(
        &mut self,
        view: &mut jit::JitCompileSnapshot,
        context: &ExecutionContext,
        fid: u32,
    ) -> Option<()> {
        let realm_id = self.vm.function_realm_id(fid);
        view.literal_allocations.realm_id = realm_id;
        view.literal_allocations.group_allowed = self.vm.gc_heap.machine_allocation_allowed();
        for (pc, instruction) in view.instructions.iter().enumerate() {
            match instruction.op(&view.code_block) {
                // A literal in the default realm needs no prototype override.
                Op::LoadRegExp if realm_id == 0 => {
                    let Some(constant) = instruction.const_index(&view.code_block, 1) else {
                        continue;
                    };
                    let owner = context.for_function(fid).ok()?;
                    let key = owner.constant_cache_key(constant);
                    // The literal's first execution resolves its site.
                    let Some(&site) = self.vm.regexp_literals().get(&key) else {
                        continue;
                    };
                    view.literal_allocations.regexps.insert(
                        instruction.byte_pc,
                        jit::JitRegExpLiteralPlan::new(constant, site),
                    );
                }
                op if op == Op::NewObject
                    || (matches!(op, Op::Call | Op::New)
                        && instruction.const_index(&view.code_block, 2) == Some(0)
                        && matches!(
                            view.code_block.call_distribution_at(pc),
                            Some(feedback::CallSiteDistribution::Mono(
                                feedback::CallTargetCount {
                                    target: feedback::OrdinaryCallTarget::Intrinsic(
                                        jit::IntrinsicCallKind::Object
                                    ),
                                    ..
                                }
                            ))
                        )) =>
                {
                    // The first `{}` of the realm gave `%Object.prototype%` its
                    // instance root; a foreign realm's literal keeps its
                    // allocating entry.
                    if realm_id != self.vm.active_realm_id {
                        continue;
                    }
                    let root = self.vm.object_prototype_object_opt().and_then(|prototype| {
                        crate::object::cached_instance_root(prototype, &self.vm.gc_heap)
                    });
                    view.literal_allocations.object = root
                        .and_then(|root| {
                            crate::object::shape_body::root_for_layout(
                                root,
                                crate::object::DEFAULT_INLINE_CAPACITY,
                                crate::object::ShapeState::ORDINARY,
                            )
                        })
                        .and_then(|root| self.bake_shape(root))
                        .map(jit::JitEmptyObjectAllocationPlan::new);
                }
                Op::SetPrototype if view.literal_allocations.null_object.is_none() => {
                    let root = self.vm.null_prototype_root();
                    view.literal_allocations.null_object = self
                        .bake_shape(root)
                        .map(jit::JitEmptyObjectAllocationPlan::new);
                }
                Op::NewObjectLiteral => {
                    let site = self
                        .vm
                        .registered_jit_allocation_site(view.code_block.allocation_site_at(pc));
                    view.literal_allocations
                        .sites
                        .insert(u32::try_from(pc).ok()?, site);
                    let count = instruction.const_index(&view.code_block, 1)? as usize;
                    let first_key = instruction.const_index(&view.code_block, 2)?;
                    // The literal's first execution built its layout.
                    let plan = self
                        .vm
                        .object_literal_layouts()
                        .get(&(fid, first_key))
                        .and_then(|layout| self.shape_for_id(layout.shape_id()))
                        .and_then(|shape| {
                            self.bake_shape(shape)?;
                            jit::JitObjectLiteralAllocationPlan::new(shape, count)
                        });
                    if let Some(plan) = plan {
                        view.literal_allocations
                            .objects
                            .insert(u32::try_from(pc).ok()?, plan);
                    }
                }
                Op::NewArrayBoilerplate => {
                    let site = self
                        .vm
                        .registered_jit_allocation_site(view.code_block.allocation_site_at(pc));
                    view.literal_allocations
                        .sites
                        .insert(u32::try_from(pc).ok()?, site);
                    let constant = instruction.const_index(&view.code_block, 1)?;
                    // The literal's first execution built its shared slab.
                    let slab = context
                        .array_boilerplate_for_function(fid, constant)
                        .and_then(|(key, _)| self.vm.array_boilerplates().get(&key).copied());
                    if let Some(plan) = slab.and_then(jit::JitBoilerplateArrayAllocationPlan::new) {
                        view.literal_allocations
                            .boilerplates
                            .insert(u32::try_from(pc).ok()?, plan);
                    }
                }
                // A `+` that concatenated places its strings at its site.
                Op::Add | Op::AddImm
                    if instruction.arith_feedback().bits() & crate::jit_feedback::ARITH_STRING
                        != 0 =>
                {
                    let site = self
                        .vm
                        .registered_jit_allocation_site(view.code_block.allocation_site_at(pc));
                    view.literal_allocations
                        .sites
                        .insert(u32::try_from(pc).ok()?, site);
                }
                Op::NewArray => {
                    let count = instruction.const_index(&view.code_block, 1)? as usize;
                    if count != 0 {
                        let site = self
                            .vm
                            .registered_jit_allocation_site(view.code_block.allocation_site_at(pc));
                        view.literal_allocations
                            .sites
                            .insert(u32::try_from(pc).ok()?, site);
                    }
                    if let Some(plan) = jit::JitArrayLiteralAllocationPlan::new(count) {
                        view.literal_allocations
                            .arrays
                            .insert(u32::try_from(pc).ok()?, plan);
                    }
                }
                _ => {}
            }
        }
        Some(())
    }

    /// Plan inline context allocation for every scope of this function and
    /// of every body inlined into it.
    pub(crate) fn bake_context_allocations(view: &mut jit::JitCompileSnapshot) {
        fn collect(
            view: &jit::JitCompileSnapshot,
            plans: &mut rustc_hash::FxHashMap<(u32, u32), jit::JitContextAllocationPlan>,
            depth: u32,
        ) {
            let code_block = &view.code_block;
            for scope in 0..code_block.scopes.len() {
                let Ok(scope) = u32::try_from(scope) else {
                    break;
                };
                if let Some(plan) = jit::context_allocation_plan(code_block, scope) {
                    plans.insert((code_block.id, scope), plan);
                }
            }
            // Inline nesting is bounded by the inliner; the cap only guards
            // a malformed snapshot graph.
            if depth >= 16 {
                return;
            }
            for callee in view.inline_callees.values() {
                collect(&callee.body, plans, depth + 1);
            }
            for method in view.method_targets.values().flatten() {
                if let Some(body) = &method.body {
                    collect(body, plans, depth + 1);
                }
            }
        }
        let mut plans = rustc_hash::FxHashMap::default();
        collect(view, &mut plans, 0);
        view.context_allocations = plans;
    }

    /// Plan inline closure allocation for every `MakeClosure` and
    /// `MakeFunction` site of this function. `context` owns the function.
    pub(crate) fn bake_closure_allocations(
        &mut self,
        view: &mut jit::JitCompileSnapshot,
        context: &ExecutionContext,
    ) {
        let mut plans = rustc_hash::FxHashMap::default();
        let Ok(owner) = context.for_function(view.code_block.id) else {
            view.closure_allocations = plans;
            return;
        };
        for instruction in &view.instructions {
            let op = instruction.op(&view.code_block);
            if !matches!(op, Op::MakeClosure | Op::MakeFunction) {
                continue;
            }
            let Some(function_id) = instruction
                .const_index(&view.code_block, 1)
                .and_then(|index| owner.function_id_constant(index))
            else {
                continue;
            };
            // A generator or async function starts from its kind's
            // prototype, not the ordinary `%Function.prototype%` lookup; its
            // closures take the allocating runtime entry.
            let plain = owner.for_function(function_id).ok().is_some_and(|owner| {
                owner.function(function_id).is_some_and(|function| {
                    !function.is_generator && !function.is_async && !function.is_async_generator
                })
            });
            if !plain {
                continue;
            }
            let arrow = op == Op::MakeClosure && owner.function_is_arrow(function_id);
            let flags = crate::closure::CLOSURE_FLAGS_ORDINARY_LOOKUP
                | if arrow {
                    crate::closure::CLOSURE_CALL_FLAG_BOUND_THIS
                } else {
                    0
                };
            plans.insert(
                instruction.byte_pc,
                jit::JitClosureAllocationPlan {
                    call_word: u64::from(function_id) | (u64::from(flags) << 32),
                    arrow,
                    context_cell: self.vm.classified_closure_context(function_id),
                },
            );
        }
        view.closure_allocations = plans;
    }

    /// Bake fixed Array-body fields used by native guards. Element backing
    /// stores remain behind runtime stubs and are deliberately absent here.
    pub(crate) fn bake_typed_array_layout(view: &mut jit::JitCompileSnapshot) {
        let header = otter_gc::header::HEADER_SIZE as u32;
        view.array_layout = jit::JitArrayLayout::current();
        {
            use crate::binary::typed_array as typed;
            view.typed_array_length = jit::JitTypedArrayLengthLayout {
                type_tag: typed::TYPED_ARRAY_BODY_TYPE_TAG,
                expando_byte: header + typed::TYPED_ARRAY_BODY_EXPANDO_OFFSET as u32,
                lookup_start_byte: header + typed::TYPED_ARRAY_BODY_LOOKUP_START_OFFSET as u32,
                data_byte: header + typed::TYPED_ARRAY_BODY_DATA_OFFSET as u32,
                length_byte: header + typed::TYPED_ARRAY_BODY_LENGTH_OFFSET as u32,
            };
        }
        view.cage_base = otter_gc::cage_base() as usize;
    }

    /// Snapshot complete CodeBlock-owned CacheIR programs for native lowering.
    pub(crate) fn bake_property_cache_ir(
        &mut self,
        view: &mut jit::JitCompileSnapshot,
        context: &ExecutionContext,
    ) {
        view.property_action_cache = Some(self.vm.property_cache.jit_layout());
        view.backedge_fuel_cell = self.vm.interrupt.fuel_ptr() as usize;
        let sites: Vec<_> = view
            .instructions
            .iter()
            .filter(|instr| {
                matches!(
                    instr.op(&view.code_block),
                    Op::LoadProperty
                        | Op::HasNamedProperty
                        | Op::StoreProperty
                        | Op::CallMethodValue
                )
            })
            .map(|instr| {
                (
                    instr.byte_pc,
                    instr.instruction_pc(&view.code_block),
                    instr.op(&view.code_block),
                    match instr.op(&view.code_block) {
                        Op::LoadProperty | Op::HasNamedProperty | Op::CallMethodValue => {
                            instr.const_index(&view.code_block, 2)
                        }
                        Op::StoreProperty => instr.const_index(&view.code_block, 1),
                        _ => None,
                    },
                )
            })
            .collect();
        for (byte_pc, instruction_pc, op, name_index) in sites {
            // A method call's lookup shares the action cache.
            let kind = if op == Op::StoreProperty {
                crate::property_ic::PropertyIcKind::Store
            } else {
                crate::property_ic::PropertyIcKind::Load
            };
            let Some(key) = name_index.and_then(|name_index| {
                context.property_atom_for_function(view.code_block.id, name_index)
            }) else {
                continue;
            };
            let slot = view
                .code_block
                .property_feedback_at(instruction_pc as usize, kind);
            let shared = slot.is_some_and(|slot| slot.is_megamorphic());
            let atom = key.atom().id().raw();
            if let Some(slot) = slot {
                slot.native()
                    .bind_site(view.code_block.id, instruction_pc, atom);
            }
            view.property_accesses.insert(
                byte_pc,
                jit::JitPropertyAccess {
                    atom,
                    shared,
                    ic_slot: slot.map_or(0, |slot| slot.native().address()),
                },
            );
            let Some(slot) = slot else {
                continue;
            };
            if shared {
                // A megamorphic site still reads an intrinsic receiver's
                // prototype directly ahead of the shared table, which never
                // holds a non-ordinary receiver.
                if op != Op::StoreProperty {
                    let programs = self.intrinsic_property_programs(key);
                    if !programs.is_empty() {
                        view.property_programs.insert(byte_pc, programs);
                    }
                }
                continue;
            }
            let mut programs = slot.jit_programs(atom, self).unwrap_or_default();
            match op {
                // A body-slot write stands alone: a site that also wrote
                // ordinary objects keeps its shape programs.
                Op::StoreProperty => {
                    if programs.is_empty() {
                        programs.extend(super::jit_intrinsic_properties::body_slot_program(
                            key, true,
                        ));
                    }
                }
                Op::LoadProperty => {
                    programs.extend(self.intrinsic_property_programs(key));
                    programs.extend(super::jit_intrinsic_properties::body_slot_program(
                        key, false,
                    ));
                }
                // A dense-array mutation keeps its committed resolution, which
                // records the call feedback its guarded leaf is built from.
                _ => programs.extend(self.intrinsic_property_programs(key).into_iter().filter(
                    |program| {
                        !matches!(key.name(), "push" | "pop" | "shift" | "unshift")
                            || !program.reads_array_receiver()
                    },
                )),
            }
            if !programs.is_empty() {
                view.property_programs.insert(byte_pc, programs);
            }
        }
    }

    /// Export the named programs of each element site whose cache serves one
    /// name key, with that key, which the generated access proves: a symbol
    /// by its atom, an integer index by its value.
    pub(crate) fn bake_keyed_name_sites(&mut self, view: &mut jit::JitCompileSnapshot) {
        let sites: Vec<_> = view
            .instructions
            .iter()
            .filter_map(|instr| {
                let kind = match instr.op(&view.code_block) {
                    Op::LoadElement => crate::property_ic::PropertyIcKind::Load,
                    Op::StoreElement | Op::StoreElementStrict => {
                        crate::property_ic::PropertyIcKind::Store
                    }
                    _ => return None,
                };
                Some((instr.byte_pc, instr.instruction_pc(&view.code_block), kind))
            })
            .collect();
        for (byte_pc, instruction_pc, kind) in sites {
            let Some(slot) = view
                .code_block
                .property_feedback_at(instruction_pc as usize, kind)
            else {
                continue;
            };
            let Some(atom) = slot.native().keyed_atom() else {
                continue;
            };
            let names = crate::isolate_names::names(&self.vm.gc_heap);
            let name = match names.spelling(crate::property_atom::AtomId::from_global(atom)) {
                None => jit::JitKeyedName::Symbol { atom },
                Some(spelling) => {
                    let Some(index) = crate::object::array_index_property_name(spelling)
                        .and_then(|index| i32::try_from(index).ok())
                    else {
                        continue;
                    };
                    jit::JitKeyedName::Index { index }
                }
            };
            let programs = slot.jit_programs(atom, self).unwrap_or_default();
            if programs.is_empty() {
                continue;
            }
            view.property_accesses.insert(
                byte_pc,
                jit::JitPropertyAccess {
                    atom,
                    shared: false,
                    ic_slot: slot.native().address(),
                },
            );
            view.property_programs.insert(byte_pc, programs);
            view.keyed_names.insert(byte_pc, name);
        }
    }

    /// Describe how each `LoadElement` / `StoreElement` site addresses its
    /// receiver's elements, and an `IteratorNext` site the array its fast
    /// record steps.
    ///
    /// The family comes from what the site observed, so a typed view and a
    /// dense array are the same program over different declared offsets. A
    /// store additionally guards that the value already has the view's
    /// representation, because coercing it could run user code. A site that
    /// reached past its receiver's dense elements is listed as such.
    pub(crate) fn bake_element_accesses(&mut self, view: &mut jit::JitCompileSnapshot) {
        self.bake_array_iteration(view);
        let sites: Vec<_> = view
            .instructions
            .iter()
            .filter_map(|instr| {
                let op = instr.op(&view.code_block);
                matches!(
                    op,
                    Op::LoadElement | Op::StoreElement | Op::StoreElementStrict | Op::IteratorNext
                )
                .then_some((
                    instr.byte_pc,
                    instr.instruction_pc(&view.code_block),
                    matches!(op, Op::StoreElement | Op::StoreElementStrict),
                ))
            })
            .collect();
        for (byte_pc, pc, write) in sites {
            let cell = view.code_block.feedback_at(pc as usize);
            if cell.is_some_and(|cell| cell.element_out_of_bounds()) {
                view.out_of_bounds_element_sites.insert(byte_pc);
            }
            let family = cell.map_or(jit::JitElementFamily::Unseen, |cell| cell.element_family());
            let access = match family {
                jit::JitElementFamily::Typed(kind) => {
                    let Some(element) = jit::JitElementRepr::for_typed_kind(kind) else {
                        continue;
                    };
                    Self::typed_element_access(kind, element)
                }
                jit::JitElementFamily::DenseTagged
                    if !write && view.out_of_bounds_element_sites.contains(&byte_pc) =>
                {
                    Self::absent_reading_element_access()
                }
                jit::JitElementFamily::DenseTagged => Self::dense_element_access(
                    jit::JitElementRepr::Boxed,
                    crate::array::DENSE_ELEMENT_KIND_TAGGED,
                    write,
                ),
                jit::JitElementFamily::DenseFloat64 => Self::dense_element_access(
                    jit::JitElementRepr::Float64,
                    crate::array::DENSE_ELEMENT_KIND_PACKED_DOUBLE,
                    write,
                ),
                jit::JitElementFamily::DenseHoleyFloat64 => {
                    Self::holey_double_element_access(write)
                }
                jit::JitElementFamily::Unseen => {
                    view.unseen_element_sites.insert(byte_pc);
                    continue;
                }
                jit::JitElementFamily::Generic => continue,
            };
            view.element_accesses.insert(byte_pc, access);
        }
    }

    /// The realm's Array iteration proof cells, for a function that opens or
    /// closes a synchronous iterator record while the proof holds.
    fn bake_array_iteration(&mut self, view: &mut jit::JitCompileSnapshot) {
        let iterates = view.instructions.iter().any(|instr| {
            matches!(
                instr.op(&view.code_block),
                Op::GetIterator | Op::IteratorClose | Op::IteratorCloseThrow
            )
        });
        if !iterates {
            return;
        }
        let Some(cells) = self.vm.proven_array_iteration_cells() else {
            return;
        };
        let (Some(iterable), Some(iterator)) = (
            self.bake_prototype_validity(&cells.iterable),
            self.bake_prototype_validity(&cells.iterator),
        ) else {
            return;
        };
        view.array_iteration = Some(jit::JitArrayIteration {
            iterable,
            iterator,
            close: cells.close,
            realm: self.vm.active_realm_id,
            array_type_tag: crate::array::ARRAY_BODY_TYPE_TAG,
            receiver_guard: crate::method_ops::array_named_lookup_guard(),
        });
    }

    /// Describe one exact present-own array representation behind the body's
    /// element cache. Prototype-only sidecars are legal; descriptor/accessor
    /// baggage and a numeric-to-tagged transition leave before access. A read
    /// also accepts storage shared with a literal boilerplate; a write leaves
    /// for the runtime, which copies it out first.
    fn dense_element_access(
        element: jit::JitElementRepr,
        dense_kind: u32,
        write: bool,
    ) -> jit::JitElementAccess {
        let header = otter_gc::header::HEADER_SIZE as u32;
        let own = header + crate::array::ARRAY_BODY_DENSE_OWN_GUARD_OFFSET as u32;
        jit::JitElementAccess {
            type_tag: crate::array::ARRAY_BODY_TYPE_TAG,
            guards: [
                Some(if write {
                    jit::JitBodyGuard::clear(own, jit::JitGuardWidth::Byte)
                } else {
                    jit::JitBodyGuard::clear_bit(
                        own,
                        jit::JitGuardWidth::Byte,
                        crate::array::DENSE_OWN_GUARD_EXOTIC.trailing_zeros(),
                    )
                }),
                Some(jit::JitBodyGuard {
                    byte: header + crate::array::ARRAY_BODY_DENSE_KIND_OFFSET as u32,
                    width: jit::JitGuardWidth::Byte,
                    expect: dense_kind,
                    mask: jit::JitBodyGuard::WHOLE,
                }),
            ],
            length_byte: header + crate::array::ARRAY_BODY_DENSE_LEN_OFFSET as u32,
            length_width: jit::JitGuardWidth::Word32,
            base: jit::JitElementBase::InBody {
                byte: header + crate::array::ARRAY_BODY_ELEMENTS_PTR_OFFSET as u32,
            },
            element,
            holes: None,
            reads_absent: false,
        }
    }

    /// Tagged dense storage of a read site that met holes or indices past the
    /// elements: the exotic sidecar itself is guarded null, which proves no
    /// prototype override, sparse element or indexed accessor besides the
    /// present own slots, so an absent index reads the realm element
    /// prototypes alone.
    fn absent_reading_element_access() -> jit::JitElementAccess {
        let header = otter_gc::header::HEADER_SIZE as u32;
        let mut access = Self::dense_element_access(
            jit::JitElementRepr::Boxed,
            crate::array::DENSE_ELEMENT_KIND_TAGGED,
            false,
        );
        access.guards[0] = Some(jit::JitBodyGuard::clear(
            header + std::mem::offset_of!(crate::array::ArrayBody, exotic) as u32,
            jit::JitGuardWidth::Word32,
        ));
        access.reads_absent = true;
        access
    }

    /// Numeric dense storage of either kind, read through the hole bitmap.
    /// The exotic sidecar is guarded as for every dense family; the storage
    /// kind is proved by the bitmap descriptor, which admits both numeric
    /// kinds. A write also requires private storage.
    fn holey_double_element_access(write: bool) -> jit::JitElementAccess {
        let header = otter_gc::header::HEADER_SIZE as u32;
        jit::JitElementAccess {
            guards: [
                Some(jit::JitBodyGuard::clear(
                    header + std::mem::offset_of!(crate::array::ArrayBody, exotic) as u32,
                    jit::JitGuardWidth::Word32,
                )),
                write.then(|| {
                    jit::JitBodyGuard::clear(
                        header + crate::array::ARRAY_BODY_DENSE_OWN_GUARD_OFFSET as u32,
                        jit::JitGuardWidth::Byte,
                    )
                }),
            ],
            holes: Some(jit::JitArrayStorage::current().numeric),
            ..Self::dense_element_access(
                jit::JitElementRepr::Float64,
                crate::array::DENSE_ELEMENT_KIND_HOLEY_DOUBLE,
                write,
            )
        }
    }

    /// A typed view's elements are raw scalars in its backing buffer. The
    /// view's own cached length is a construction-time field that a detach
    /// leaves untouched, so the buffer's detached flag is guarded on the way
    /// through rather than inferred from the bounds check; a length-tracking
    /// view over a resizable buffer has a stale cached length and leaves the
    /// fast path outright.
    fn typed_element_access(
        kind: crate::binary::TypedArrayKind,
        element: jit::JitElementRepr,
    ) -> jit::JitElementAccess {
        use crate::binary::array_buffer as buffer;
        use crate::binary::typed_array as view;
        let header = otter_gc::header::HEADER_SIZE as u32;
        let buffer_byte = header + view::TYPED_ARRAY_BODY_BUFFER_OFFSET as u32;
        jit::JitElementAccess {
            type_tag: view::TYPED_ARRAY_BODY_TYPE_TAG,
            guards: [
                Some(jit::JitBodyGuard {
                    byte: header + view::TYPED_ARRAY_BODY_KIND_OFFSET as u32,
                    width: jit::JitGuardWidth::Word32,
                    expect: kind as u32,
                    mask: jit::JitBodyGuard::WHOLE,
                }),
                Some(jit::JitBodyGuard::clear(
                    header + view::TYPED_ARRAY_BODY_LENGTH_TRACKING_OFFSET as u32,
                    jit::JitGuardWidth::Byte,
                )),
            ],
            length_byte: header + view::TYPED_ARRAY_BODY_LENGTH_OFFSET as u32,
            length_width: jit::JitGuardWidth::Word64,
            base: jit::JitElementBase::ThroughLocalBuffer {
                storage_tag_byte: buffer_byte + buffer::BUFFER_STORAGE_DISCRIMINANT_OFFSET as u32,
                local_tag: buffer::BUFFER_STORAGE_LOCAL_TAG,
                handle_byte: buffer_byte + buffer::BUFFER_STORAGE_HANDLE_OFFSET as u32,
                detached_byte: header + buffer::LOCAL_ARRAY_BUFFER_BODY_DETACHED_OFFSET as u32,
                data_ptr_byte: header + buffer::LOCAL_ARRAY_BUFFER_BODY_DATA_OFFSET as u32,
                byte_len_byte: header + buffer::LOCAL_ARRAY_BUFFER_BODY_BYTE_LEN_OFFSET as u32,
                view_offset_byte: header + view::TYPED_ARRAY_BODY_BYTE_OFFSET_OFFSET as u32,
                cached_data_byte: header + view::TYPED_ARRAY_BODY_DATA_OFFSET as u32,
            },
            element,
            holes: None,
            reads_absent: false,
        }
    }

    /// Bake the static heap-layout offsets for inline primitive string fast
    /// paths. String bodies are GC cells addressed through the same cage base as
    /// object/array bodies, so this only enables when the compile snapshot has a
    /// cage base.
    pub(crate) fn bake_string_layout(view: &mut jit::JitCompileSnapshot) {
        view.string_layout = jit::JitStringLayout::default();
        view.cage_base = otter_gc::cage_base() as usize;
    }

    /// Bake every `Op::CallMethodValue` site whose callee is a declared native
    /// entry into the compile snapshot.
    ///
    /// The receiver layout comes from the same feedback a property site
    /// records, so generated code guards it with the shared way walk and
    /// prototype hop rather than a second description of the same access. An
    /// entry that may collect reserves its safepoint here, at the site that
    /// will publish it.
    /// Proof for an `f.call(...)` site whose feedback recorded the function
    /// `%Function.prototype.call%` ran. A method call named `call` proves its
    /// lookup through the closure property program while the intrinsic still
    /// occupies the slot; an explicit-receiver call proves only its loaded
    /// callee. Generated code re-proves the intrinsic's identity before
    /// calling the function.
    fn function_prototype_call_proof(
        &mut self,
        view: &jit::JitCompileSnapshot,
        context: &ExecutionContext,
        op: Op,
        instruction_pc: u32,
    ) -> Option<jit::JitFunctionPrototypeCall> {
        let intrinsic = crate::native_function::VmIntrinsicFunction::FunctionPrototypeCall;
        let call_native_ref = intrinsic.native_ref(&self.vm.gc_heap)?;
        let lookup = match op {
            Op::CallWithThis => None,
            Op::CallMethodValue => {
                let name_index = view
                    .instructions
                    .get(instruction_pc as usize)?
                    .const_index(&view.code_block, 2)?;
                let key = context.property_atom_for_function(view.code_block.id, name_index)?;
                let holds_intrinsic = self
                    .vm
                    .realm_intrinsics
                    .function_prototype()
                    .and_then(|prototype| object::get(prototype, &self.vm.gc_heap, "call"))
                    .and_then(|value| value.as_native_function())
                    .is_some_and(|native| native.is_vm_intrinsic(&self.vm.gc_heap, intrinsic));
                if key.name() != "call" || !holds_intrinsic {
                    return None;
                }
                // The closure program is one of the name's prepared
                // intrinsic programs: the one reading `%Function.prototype%`.
                let program =
                    self.intrinsic_property_programs(key)
                        .into_iter()
                        .find(|program| {
                            matches!(
                                program.ops.first(),
                                Some(jit::JitCacheIrOp::LoadIntrinsicPrototype { target, .. })
                                    if target.type_tag == crate::closure::JS_CLOSURE_BODY_TYPE_TAG
                            )
                        })?;
                let [
                    jit::JitCacheIrOp::LoadIntrinsicPrototype { target, .. },
                    jit::JitCacheIrOp::GuardShape { shape, .. },
                    _,
                    jit::JitCacheIrOp::LoadField { field, .. },
                ] = *program.ops
                else {
                    return None;
                };
                Some(jit::JitFunctionCallLookup {
                    receiver: target,
                    holder_shape: shape,
                    call_field: field,
                })
            }
            _ => return None,
        };
        Some(jit::JitFunctionPrototypeCall {
            lookup,
            call_native_ref,
        })
    }

    pub(crate) fn bake_guarded_method_calls(
        &mut self,
        view: &mut jit::JitCompileSnapshot,
        context: &ExecutionContext,
        fid: u32,
    ) {
        let sites: Vec<_> = view
            .instructions
            .iter()
            .filter(|instr| instr.op(&view.code_block) == Op::CallMethodValue)
            .map(|instr| {
                (
                    instr.byte_pc,
                    instr.property_ic_site(&view.code_block),
                    instr.const_index(&view.code_block, 2),
                )
            })
            .collect();
        for (byte_pc, site, name_index) in sites {
            let alloc_safepoint_id = view.safepoints.len() as native_abi::SafepointId;
            // An exotic receiver — a collection body, a dense array or a
            // primitive — reaches its builtin through a pinned realm prototype
            // rather than a shape, so its feedback names the call directly.
            if let Some(mut call) = site
                .and_then(|site| self.jit_collection_method_call(site, alloc_safepoint_id))
                .or_else(|| {
                    site.and_then(|site| self.jit_array_method_call(site, alloc_safepoint_id))
                })
                .or_else(|| {
                    let name = context.string_constant_str_for_function(fid, name_index?)?;
                    self.jit_primitive_method_call(name)
                })
            {
                if reserve_guarded_entry_safepoint(view, &mut call) {
                    view.guarded_method_calls.insert(byte_pc, call);
                }
                continue;
            }
            let Some(site) = site else {
                continue;
            };
            let Some(MethodCallFeedback::MonoNativeLeaf {
                stub_id,
                method_field,
                recv_shape,
                prototype,
            }) = self.vm.method_target_feedback(site)
            else {
                continue;
            };
            // Feedback names shapes by id; a shape collected since then leaves
            // the site on the runtime path.
            let Some(recv_shape_handle) = self.shape_for_id(recv_shape) else {
                continue;
            };
            let holder = match &prototype.validity {
                None => jit::JitMethodHolder::Receiver,
                Some(cell) => {
                    let Some(validity) = self.bake_prototype_validity(cell) else {
                        continue;
                    };
                    let Some(root) = self.bake_shape_id(prototype.holder_root) else {
                        continue;
                    };
                    jit::JitMethodHolder::Prototype { validity, root }
                }
            };
            let Some(declaration) = crate::jit_static_native::jit_leaf_builtin(stub_id)
                .filter(|declaration| !declaration.this_operand)
            else {
                continue;
            };
            // A builtin this isolate never installed has no external-ref
            // index, so no live receiver could carry its identity.
            let Some(builtin_native_ref) =
                crate::jit_static_native::jit_static_call_ref(stub_id, &self.vm.gc_heap)
            else {
                continue;
            };
            let Some(shape) = self.bake_shape(recv_shape_handle) else {
                continue;
            };
            view.guarded_method_calls.insert(
                byte_pc,
                jit::JitGuardedMethodCall {
                    receiver: jit::JitGuardedReceiver::Shape { shape },
                    holder,
                    method_field,
                    builtin_native_ref,
                    entry_stub_id: stub_id,
                    safepoint_id: native_abi::NO_SAFEPOINT,
                    argument_count: declaration.argument_count,
                    grow: None,
                    holder_validity: None,
                },
            );
        }
    }

    /// Bake direct reads for global-declarative bindings and guarded own-data
    /// slots already owned by the isolate's global object record.
    ///
    /// Global lexical cells are old-space, non-moving GC objects rooted for the
    /// lifetime of the binding. Their identity cannot be replaced by later
    /// declarations, while their contained `Value` remains mutable. Generated
    /// code may therefore read the live cell directly; a TDZ hole still enters
    /// the canonical `LoadGlobalOrThrow` stub to construct the named error.
    /// Object-record reads additionally guard the live declarative-record epoch
    /// and the global object's hidden class or dictionary slot layout, so later
    /// eval/script lexicals, deletions and redefinitions miss before reading
    /// the baked slot; appending unrelated globals keeps the proof.
    fn bake_global_lexical_loads(
        &mut self,
        view: &mut jit::JitCompileSnapshot,
        context: &ExecutionContext,
        fid: u32,
    ) {
        // The linked FID owns its realm. A foreign call may reach compilation
        // while the caller's realm is active; those cells cannot prove this
        // source's bindings. Keep the source-aware committed operation instead.
        if self.vm.foreign_function_realm(fid).is_some() {
            return;
        }
        for instruction in &view.instructions {
            if instruction.op(&view.code_block) != Op::LoadGlobalOrThrow {
                continue;
            }
            let Some(name_index) = instruction.const_index(&view.code_block, 1) else {
                continue;
            };
            let Some(name) = context.string_constant_str_for_function(fid, name_index) else {
                continue;
            };
            if let Some(&(cell, _)) = self.vm.global_lexicals().get(name) {
                view.global_lexical_loads.insert(
                    instruction.byte_pc,
                    jit::JitGlobalLexicalLoad {
                        cell_offset: cell.offset(),
                    },
                );
                continue;
            }
            let (Some(hit), crate::object::PropertyLookup::Data { .. }) =
                crate::object::lookup_own_slot(self.vm.global_this, &self.vm.gc_heap, name)
            else {
                continue;
            };
            let state = crate::object::state(self.vm.global_this, &self.vm.gc_heap);
            if state.is_opaque() {
                continue;
            }
            let shape = crate::object::keyed_shape(self.vm.global_this, &self.vm.gc_heap);
            let (shape, dictionary) = if shape.is_null() {
                // A dictionary global object is proven by its slot layout, so
                // globals the program adds later do not retire the proof. The
                // generated read trusts this slot's kind; watching it makes a
                // redefinition advance the layout the proof guards.
                let Some(layout) =
                    crate::object::dictionary_layout(self.vm.global_this, &self.vm.gc_heap)
                else {
                    continue;
                };
                // The isolate watches the slot before publishing the code,
                // once it re-proves the layout is still this one.
                self.install.global_slot_watches.push(GlobalSlotWatch {
                    slot: hit.slot,
                    layout,
                });
                (u64::from(layout), true)
            } else {
                let Some(shape) = self.bake_shape(shape) else {
                    continue;
                };
                (u64::from(shape), false)
            };
            view.global_object_loads.insert(
                instruction.byte_pc,
                jit::JitGlobalObjectLoad {
                    shape,
                    dictionary,
                    field: crate::object::field_location_at(
                        self.vm.global_this,
                        &self.vm.gc_heap,
                        u32::from(hit.slot),
                    ),
                    global_lexical_epoch: self.vm.global_lexical_epoch(),
                },
            );
        }
    }

    /// Bake direct hit proofs for global-declarative bindings and guarded
    /// own-data slots already owned by the isolate's global object record.
    ///
    /// Global lexical cells are old-space, non-moving GC objects rooted for the
    /// lifetime of the binding. Their identity cannot be replaced by later
    /// declarations, while their contained `Value` remains mutable. Generated
    /// code may therefore read the live cell directly; a TDZ hole enters the
    /// schema-decoded committed binding boundary to construct the named error.
    /// Object-record reads additionally guard the live declarative-record epoch
    /// and the global object's hidden class or dictionary slot layout, so later
    /// eval/script lexicals, deletions and redefinitions miss before reading
    /// the baked slot; appending unrelated dictionary globals keeps the proof.
    /// Ordinary hits prove one eligible finalized immutable shape. Descriptor,
    /// extensibility and lookup-state changes install another identity. Writes
    /// refuse prototype-role shapes at admission; dictionary hits also prove
    /// their dynamic state and watched slot layout before any effect.
    fn bake_binding_hit_proofs(
        &mut self,
        view: &mut jit::JitCompileSnapshot,
        context: &ExecutionContext,
        fid: u32,
    ) {
        // The linked FID owns its realm. A foreign call may reach compilation
        // while the caller's realm is active; those cells cannot prove this
        // source's bindings. Keep the source-aware committed operation instead.
        if self.vm.foreign_function_realm(fid).is_some() {
            return;
        }
        for instruction in &view.instructions {
            let op = instruction.op(&view.code_block);
            let Some(binding) = otter_bytecode::opcode_schema::opcode_schema(op).binding else {
                continue;
            };
            let (name_operand, writing) = match binding {
                otter_bytecode::opcode_schema::BindingSemantics::Read(
                    otter_bytecode::opcode_schema::BindingRead::Global { name, .. }
                    | otter_bytecode::opcode_schema::BindingRead::Exists { name, .. },
                ) => (name, false),
                otter_bytecode::opcode_schema::BindingSemantics::Write(
                    otter_bytecode::opcode_schema::BindingWrite::Global { name, .. }
                    | otter_bytecode::opcode_schema::BindingWrite::GlobalChecked { name, .. },
                ) => (name, true),
                _ => continue,
            };
            let Some(name_index) =
                instruction.const_index(&view.code_block, usize::from(name_operand))
            else {
                continue;
            };
            let Some(name) = context.string_constant_str_for_function(fid, name_index) else {
                continue;
            };
            if let Some(&(cell, is_const)) = self.vm.global_lexicals().get(name) {
                view.binding_hit_proofs.insert(
                    instruction.byte_pc,
                    jit::BindingHitProof::GlobalLexical {
                        cell_offset: cell.offset(),
                        writable: !is_const,
                    },
                );
                continue;
            }
            let (Some(hit), crate::object::PropertyLookup::Data { flags, .. }) =
                crate::object::lookup_own_slot(self.vm.global_this, &self.vm.gc_heap, name)
            else {
                continue;
            };
            let state = crate::object::state(self.vm.global_this, &self.vm.gc_heap);
            if state.is_opaque() || (writing && state.is_prototype()) {
                continue;
            }
            let shape = crate::object::keyed_shape(self.vm.global_this, &self.vm.gc_heap);
            let (shape, dictionary) = if shape.is_null() {
                // A dictionary global object is proven by its slot layout, so
                // globals the program adds later do not retire the proof. The
                // generated read trusts this slot's kind; watching it makes a
                // redefinition advance the layout the proof guards.
                let Some(layout) =
                    crate::object::dictionary_layout(self.vm.global_this, &self.vm.gc_heap)
                else {
                    continue;
                };
                // The isolate watches the slot before publishing the code,
                // once it re-proves the layout is still this one.
                self.install.global_slot_watches.push(GlobalSlotWatch {
                    slot: hit.slot,
                    layout,
                });
                (u64::from(layout), true)
            } else {
                let Some(shape) = self.bake_shape(shape) else {
                    continue;
                };
                (u64::from(shape), false)
            };
            let layout = dictionary
                .then(|| {
                    crate::object::current_dictionary_layout_validity(
                        self.vm.global_this,
                        &self.vm.gc_heap,
                    )
                })
                .flatten()
                .and_then(|cell| self.bake_prototype_validity(&cell));
            view.binding_hit_proofs.insert(
                instruction.byte_pc,
                jit::BindingHitProof::GlobalObject {
                    shape,
                    dictionary,
                    field: crate::object::field_location_at(
                        self.vm.global_this,
                        &self.vm.gc_heap,
                        u32::from(hit.slot),
                    ),
                    global_lexical_epoch: self.vm.global_lexical_epoch(),
                    writable: flags.writable(),
                    layout,
                },
            );
        }
    }

    /// Publish direct reads of already-canonical string and BigInt literals.
    ///
    /// The isolate owns boxed `Value` cells: hash-table growth may move a box
    /// but never its allocation, and root tracing rewrites the cell after a
    /// moving collection. Every published site is therefore a leaf relocation
    /// load; cold materialization happens on a literal's first execution or
    /// in the template session's preparation, never here. With `complete`,
    /// a literal without its cell refuses the whole body (the template tier
    /// emits no generic literal); otherwise that site stays generic.
    fn bake_literal_cells(
        &mut self,
        view: &mut jit::JitCompileSnapshot,
        context: &ExecutionContext,
        fid: u32,
        complete: bool,
    ) -> Option<()> {
        let owner = context.for_function(fid).ok()?;
        let cells = self.vm.compile_shared.literal_cells.try_read()?;
        for instruction in &view.instructions {
            let op = instruction.op(&view.code_block);
            if !matches!(op, Op::LoadString | Op::LoadBigInt) {
                continue;
            }
            let constant = instruction.const_index(&view.code_block, 1)?;
            let key = owner.constant_cache_key(constant);
            let Some(cell) = cells.get(&key).filter(|cell| literal_matches(op, cell)) else {
                if complete {
                    return None;
                }
                continue;
            };
            view.literal_cells.insert(
                instruction.byte_pc,
                jit::JitLiteralCell {
                    cell_addr: std::ptr::from_ref::<Value>(cell.as_ref()) as usize,
                },
            );
        }
        Some(())
    }

    /// Bake one spliced body's own compile inputs.
    ///
    /// A body compiled inside another function resolves its constants, global
    /// cells, hidden classes and call plans against *itself*, exactly as it
    /// would as an outermost function. Only a source in the active realm may
    /// prepare a splice: another realm's body cannot inherit the physical
    /// caller's global cells. Nested candidates share the root budget and use
    /// this body's own feedback; the compiler decides which bodies to splice.
    fn bake_inline_body(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
        tier: jit_debug::JitDebugTier,
        budget: &mut InlineSnapshotBudget,
        call_byte_pc: Option<u32>,
        site_frequency: Option<f32>,
        receiver: Option<ConstructReceiver>,
    ) -> Option<std::sync::Arc<jit::JitCompileSnapshot>> {
        // An inline body shares the physical caller's active-global cells.
        // Source identity alone does not make a foreign realm's globals active.
        // Refuse the optional splice before preparing any ambient proofs.
        let instructions = context
            .exec_function(fid)
            .map_or(u32::MAX, |function| function.instruction_count());
        if self.vm.jit_retraining_blocks(fid)
            || self.vm.foreign_function_realm(fid).is_some()
            || !budget.enter(call_byte_pc, fid, instructions, site_frequency)
        {
            return None;
        }
        let result = (|| {
            let mut body = context.jit_compile_snapshot(fid)?;
            self.bake_literal_allocations(&mut body, context, fid)?;
            Self::bake_typed_array_layout(&mut body);
            Self::bake_string_layout(&mut body);
            self.bake_literal_cells(
                &mut body,
                context,
                fid,
                tier == jit_debug::JitDebugTier::Template,
            )?;
            self.bake_global_lexical_loads(&mut body, context, fid);
            self.bake_binding_hit_proofs(&mut body, context, fid);
            self.bake_property_cache_ir(&mut body, context);
            self.bake_call_site_plans(&mut body, context, fid, tier, budget);
            if let Some(receiver) = receiver {
                self.bake_receiver_methods(&mut body, context, receiver, tier, budget);
            }
            self.bake_guarded_method_calls(&mut body, context, fid);
            self.bake_instanceof_cells(&mut body);
            self.bake_forward_apply(&mut body);
            self.bake_element_accesses(&mut body);
            self.bake_keyed_name_sites(&mut body);
            Self::bake_context_allocations(&mut body);
            self.bake_closure_allocations(&mut body, context);
            if tier == jit_debug::JitDebugTier::Optimizing {
                self.bake_unique_closure_context(&mut body);
            }
            self.bake_optimized_exit_profile(&mut body, fid);
            Some(std::sync::Arc::new(body))
        })();
        budget.leave();
        result
    }

    /// Name `%Function.prototype.apply%` for a body that forwards its
    /// arguments, so generated code can prove a forwarding site's method.
    fn bake_forward_apply(&mut self, view: &mut jit::JitCompileSnapshot) {
        let forwards = view
            .instructions
            .iter()
            .any(|instruction| instruction.op(&view.code_block) == Op::CallForwardArguments);
        if forwards {
            view.forward_apply_native_ref =
                crate::native_function::VmIntrinsicFunction::FunctionPrototypeApply
                    .native_ref(&self.vm.gc_heap);
        }
    }

    /// Bake the typed reasons at which earlier optimized generations of `fid`
    /// exited, and widen the baked feedback of those instructions.
    ///
    /// The exit profile is evidence the operand observations cannot carry: an
    /// int32 result that overflowed, or a value a guard refused. Folding it
    /// into the per-instruction feedback means a rebuilt generation — or a
    /// caller splicing this body inline — does not emit the speculation that
    /// already exited.
    fn bake_optimized_exit_profile(&mut self, snapshot: &mut jit::JitCompileSnapshot, fid: u32) {
        snapshot.parameter_widening = self
            .vm
            .jit_parameter_widening()
            .get(&fid)
            .cloned()
            .unwrap_or_default();
        let code_block = &snapshot.code_block;
        snapshot.optimized_exit_reasons = self
            .vm
            .exit_profiles_of(fid)
            // An insufficient-feedback exit only collected feedback, and a
            // runtime transition resumed a call no speculation guarded; the
            // site speculates again from what it has observed since. So does
            // a property site whose inline cache learned another receiver
            // program after its shape guard failed: the receiver that left
            // is now part of the feedback.
            .filter_map(|(&(_, pc, reason), profile)| {
                let relearned = reason == native_abi::ExitReason::ShapeGuard
                    && profile.feedback_population.is_some_and(|population| {
                        code_block
                            .property_site_population(pc as usize)
                            .is_some_and(|current| current != population)
                    });
                (!relearned
                    && !matches!(
                        reason,
                        native_abi::ExitReason::InsufficientFeedback
                            | native_abi::ExitReason::RuntimeTransition
                            | native_abi::ExitReason::DependencyChanged
                            | native_abi::ExitReason::AllocationSiteDecided
                    ))
                .then_some((pc, reason))
            })
            .fold(
                std::collections::BTreeMap::new(),
                |mut exits, (pc, reason)| {
                    exits.entry(pc).or_default().insert(reason);
                    exits
                },
            );
        for &pc in snapshot.optimized_exit_reasons.keys() {
            if let Some(instruction) = snapshot.instructions.get_mut(pc as usize) {
                instruction.note_optimized_exit();
            }
        }
        // A site that an earlier generation left to collect feedback has
        // executed since. An element site that still records no family meets
        // receivers no generated access describes, and is no longer unseen.
        for (&(_, pc, reason), _) in self.vm.exit_profiles_of(fid) {
            if reason == native_abi::ExitReason::InsufficientFeedback {
                snapshot.feedback_exits.insert(pc);
                if let Some(instruction) = snapshot.instructions.get(pc as usize) {
                    snapshot.unseen_element_sites.remove(&instruction.byte_pc);
                }
            }
        }
    }

    /// Bake compiler-native direct-call plans and inline-candidate bodies for
    /// `fid`'s call sites.
    ///
    /// Fixed/spread ordinary-call candidates remain monomorphic. Forwarded
    /// arguments admit the bounded ordinary target population; method calls may
    /// contain a bounded, most-frequent-first polymorphic chain. Every generated
    /// target is a synchronous bytecode function with one current non-OSR
    /// installed entry; it reaches its context through its SELF closure.
    /// Generated linkage binds
    /// both strict/lexical and unbound sloppy-global `this`; an explicitly bound
    /// sloppy closure misses before entry. Each guarded method target owns one
    /// body and its optional Template field proof; the emitter applies the final
    /// pure-leaf, size and arity checks to that same carrier.
    pub(crate) fn bake_inline_callees(
        &mut self,
        view: &mut jit::JitCompileSnapshot,
        context: &ExecutionContext,
        fid: u32,
        tier: jit_debug::JitDebugTier,
    ) {
        let mut budget = if tier == jit_debug::JitDebugTier::Optimizing {
            InlineSnapshotBudget::chosen(crate::jit_inline_plan::choose_inlined(
                &mut inline_snapshot_budget::FeedbackTree {
                    interp: self.vm,
                    context,
                    root: fid,
                },
            ))
        } else {
            InlineSnapshotBudget::new()
        };
        self.bake_call_site_plans(view, context, fid, tier, &mut budget);
    }

    /// Call-site plan baking shared by an outermost body and a spliced one.
    ///
    /// Every body owns its nested candidate tables. One depth/work
    /// budget bounds the whole tree independently of generated-entry tiering.
    fn bake_call_site_plans(
        &mut self,
        view: &mut jit::JitCompileSnapshot,
        context: &ExecutionContext,
        fid: u32,
        tier: jit_debug::JitDebugTier,
        budget: &mut InlineSnapshotBudget,
    ) {
        view.invokes = self.vm.inline_body_invokes(context, &view.code_block);
        view.call_frequencies = view
            .instructions
            .iter()
            .filter_map(|instr| {
                let index = instr.instruction_pc(&view.code_block) as usize;
                Some((instr.byte_pc, view.code_block.call_frequency_at(index)?))
            })
            .collect();
        let call_sites: Vec<_> = view
            .instructions
            .iter()
            .filter_map(|instr| {
                let instruction_pc = instr.instruction_pc(&view.code_block);
                let state = view
                    .code_block
                    .call_distribution_at(instruction_pc as usize)?;
                let op = instr.op(&view.code_block);
                Some((instruction_pc, instr.byte_pc, op, state))
            })
            .collect();
        for (instruction_pc, call_byte_pc, op, state) in call_sites {
            let is_construct = matches!(
                op,
                Op::New | Op::NewSpread | Op::SuperConstruct | Op::SuperConstructSpread
            );
            let unresolved_call_kind = match op {
                Op::New | Op::NewSpread => jit::JitDirectCallKind::Construct,
                Op::SuperConstruct | Op::SuperConstructSpread => {
                    jit::JitDirectCallKind::SuperConstruct
                }
                _ => jit::JitDirectCallKind::Plain,
            };
            let targets: Vec<_> = match state {
                feedback::CallSiteDistribution::Mono(target) => vec![target],
                // `f.call(...)` applying several functions: the site keeps
                // the intrinsic's proof and calls whichever function it gets.
                feedback::CallSiteDistribution::Poly(targets)
                    if targets.iter().all(|target| {
                        matches!(
                            target.target,
                            feedback::OrdinaryCallTarget::FunctionPrototypeCall(_)
                        )
                    }) =>
                {
                    if let Some(proof) =
                        self.function_prototype_call_proof(view, context, op, instruction_pc)
                    {
                        view.function_prototype_calls.insert(
                            call_byte_pc,
                            jit::JitFunctionPrototypeCallSite {
                                proof,
                                callee: None,
                            },
                        );
                    }
                    continue;
                }
                feedback::CallSiteDistribution::Poly(targets) if op == Op::CallForwardArguments => {
                    self.record_jit_inline_candidate(
                        fid,
                        instruction_pc,
                        tier,
                        None,
                        Some(jit_debug::JitInlineRejectionReason::Polymorphic),
                    );
                    targets.iter().copied().collect()
                }
                state => {
                    let reason = match state {
                        feedback::CallSiteDistribution::Poly(_) => {
                            jit_debug::JitInlineRejectionReason::Polymorphic
                        }
                        feedback::CallSiteDistribution::Megamorphic => {
                            jit_debug::JitInlineRejectionReason::Megamorphic
                        }
                        feedback::CallSiteDistribution::Mono(_) => unreachable!(),
                    };
                    self.record_jit_inline_candidate(fid, instruction_pc, tier, None, Some(reason));
                    continue;
                }
            };
            let target_count = targets.len() as u32;
            for (target_index, target) in targets.into_iter().enumerate() {
                let target_index = target_index as u32;
                let function_prototype_call = matches!(
                    target.target,
                    feedback::OrdinaryCallTarget::FunctionPrototypeCall(_)
                );
                let callee_fid = match target.target {
                    feedback::OrdinaryCallTarget::Native => {
                        view.native_calls
                            .insert(call_byte_pc, jit::JitNativeCall::Native);
                        continue;
                    }
                    feedback::OrdinaryCallTarget::Intrinsic(kind) => {
                        // A method intrinsic specializes `receiver.method(...)`,
                        // whether the method was loaded by the call itself or
                        // before it with the receiver kept beside it; the
                        // others, a plain call or construction.
                        let form_site = if kind.is_method() {
                            matches!(op, Op::CallMethodValue | Op::CallWithThis)
                        } else if kind == jit::IntrinsicCallKind::ReflectApply {
                            // `Reflect.apply(...)` ignores the namespace it
                            // was read from.
                            matches!(op, Op::Call | Op::CallMethodValue)
                        } else {
                            matches!(op, Op::Call | Op::New)
                        };
                        let native = if form_site
                            && self.vm.foreign_function_realm(fid).is_none()
                            && let Some(callable) = self.vm.realm_intrinsics.callable(kind)
                        {
                            let identity_cell =
                                self.bake_held_identity_cell(Value::native_function(callable));
                            jit::JitNativeCall::Intrinsic(jit::JitIntrinsicCallProof {
                                kind,
                                identity_cell,
                                realm_id: self.vm.function_realm_id(fid),
                            })
                        } else {
                            jit::JitNativeCall::Native
                        };
                        view.native_calls.insert(call_byte_pc, native);
                        continue;
                    }
                    feedback::OrdinaryCallTarget::Bytecode(callee_fid)
                    | feedback::OrdinaryCallTarget::FunctionPrototypeCall(callee_fid) => callee_fid,
                    feedback::OrdinaryCallTarget::StaticNative(stub_id) => {
                        let name = crate::native_abi::runtime_stub_name(stub_id);
                        let declaration = crate::jit_static_native::jit_leaf_builtin(stub_id)
                            .expect("native leaf call feedback names a declared entry");
                        // Feedback recorded a call to this builtin, so the isolate
                        // installed it and its external-ref index exists.
                        let builtin_native_ref = crate::jit_static_native::jit_static_call_ref(
                            stub_id,
                            &self.vm.gc_heap,
                        )
                        .expect("a builtin the site already called is interned in this isolate");
                        view.native_calls.insert(
                            call_byte_pc,
                            jit::JitNativeCall::Leaf(jit::JitStaticNativeCall {
                                builtin_native_ref,
                                leaf_stub_id: stub_id,
                                argument_count: declaration.argument_count,
                            }),
                        );
                        self.record_jit_inline_candidate(
                            fid,
                            instruction_pc,
                            tier,
                            None,
                            Some(jit_debug::JitInlineRejectionReason::StaticNative {
                                target: name,
                            }),
                        );
                        self.record_jit_static_native_call_plan(fid, instruction_pc, tier, name);
                        continue;
                    }
                };
                let Ok(callee_context) = context.for_function(callee_fid) else {
                    self.record_jit_inline_candidate(
                        fid,
                        instruction_pc,
                        tier,
                        Some(callee_fid),
                        Some(jit_debug::JitInlineRejectionReason::MissingCallee),
                    );
                    self.record_jit_direct_call_plan(
                        unresolved_call_kind,
                        fid,
                        instruction_pc,
                        tier,
                        callee_fid,
                        target_index,
                        target_count,
                        jit_debug::JitDirectCallPlanOutcome::Rejected {
                            reason: jit_debug::JitDirectCallRejectionReason::MissingCallee,
                        },
                    );
                    continue;
                };
                let Some(callee) = callee_context.exec_function(callee_fid) else {
                    self.record_jit_inline_candidate(
                        fid,
                        instruction_pc,
                        tier,
                        Some(callee_fid),
                        Some(jit_debug::JitInlineRejectionReason::MissingCallee),
                    );
                    self.record_jit_direct_call_plan(
                        unresolved_call_kind,
                        fid,
                        instruction_pc,
                        tier,
                        callee_fid,
                        target_index,
                        target_count,
                        jit_debug::JitDirectCallPlanOutcome::Rejected {
                            reason: jit_debug::JitDirectCallRejectionReason::MissingCallee,
                        },
                    );
                    continue;
                };
                let direct_ineligible = !callee.admits_generated_call(unresolved_call_kind);
                // A body that materializes `arguments`, or takes a rest list,
                // reads the actual-argument window its generated caller
                // publishes, so it keeps direct linkage; elided `arguments`
                // reads and forwarding are served from the call's values.
                let inline_ineligible =
                    direct_ineligible || !callee.splices_without_argument_frame();
                if inline_ineligible {
                    self.record_jit_inline_candidate(
                        fid,
                        instruction_pc,
                        tier,
                        Some(callee_fid),
                        Some(jit_debug::JitInlineRejectionReason::Ineligible {
                            generator: callee.is_generator,
                            async_function: callee.is_async,
                            async_generator: callee.is_async_generator,
                            needs_arguments: callee.needs_arguments,
                            has_rest: callee.has_rest,
                            contains_direct_eval: callee.contains_direct_eval,
                            derived_constructor: callee.is_derived_constructor,
                            makes_function: callee.makes_function,
                        }),
                    );
                }
                if direct_ineligible {
                    self.record_jit_direct_call_plan(
                        unresolved_call_kind,
                        fid,
                        instruction_pc,
                        tier,
                        callee_fid,
                        target_index,
                        target_count,
                        jit_debug::JitDirectCallPlanOutcome::Rejected {
                            reason: jit_debug::JitDirectCallRejectionReason::IneligibleFunction,
                        },
                    );
                    continue;
                }
                let mut construct_receiver = None;
                let direct_call_outcome = if let Some(mut plan) =
                    self.vm.current_direct_callee_plan(callee)
                {
                    debug_assert_eq!(plan.function_id, callee_fid);
                    plan.callee_cell = self.bake_callee_identity_cell();
                    let receiver_allocation = if is_construct && !callee.is_derived_constructor {
                        view.code_block
                            .construct_family_at(instruction_pc as usize)
                            .and_then(|family| {
                                self.bake_receiver_allocation_plan(callee_fid, family)
                            })
                            .map(|(allocation, receiver)| {
                                construct_receiver = Some(receiver);
                                allocation
                            })
                    } else {
                        None
                    };
                    let callee = jit::JitDirectCallee {
                        plan,
                        receiver_allocation,
                    };
                    if is_construct {
                        view.direct_constructs.insert(call_byte_pc, callee);
                    } else if function_prototype_call {
                        if let Some(proof) =
                            self.function_prototype_call_proof(view, context, op, instruction_pc)
                        {
                            // A second applied function leaves the site with
                            // the proof alone.
                            view.function_prototype_calls
                                .entry(call_byte_pc)
                                .and_modify(|site| site.callee = None)
                                .or_insert(jit::JitFunctionPrototypeCallSite {
                                    proof,
                                    callee: Some(callee),
                                });
                        }
                    } else {
                        view.direct_callees
                            .entry(call_byte_pc)
                            .or_default()
                            .push(callee);
                    }
                    jit_debug::JitDirectCallPlanOutcome::Available {
                        code_object_id: plan.code_object_id,
                        target_tier: match plan.tier {
                            native_abi::NativeFrameKind::Baseline => {
                                jit_debug::JitDebugTier::Template
                            }
                            native_abi::NativeFrameKind::Optimizing => {
                                jit_debug::JitDebugTier::Optimizing
                            }
                            native_abi::NativeFrameKind::Interpreter
                            | native_abi::NativeFrameKind::Host => {
                                jit_debug::JitDebugTier::Interpreter
                            }
                        },
                        this_mode: if is_construct && callee.plan.is_derived_constructor {
                            jit::JitDirectCallThisMode::DerivedConstructor
                        } else if is_construct {
                            jit::JitDirectCallThisMode::ConstructReceiver
                        } else {
                            plan.this_mode
                        },
                    }
                } else {
                    jit_debug::JitDirectCallPlanOutcome::Rejected {
                        reason: jit_debug::JitDirectCallRejectionReason::NoEntryGeneration,
                    }
                };
                let call_kind = match (op, callee.is_derived_constructor) {
                    (Op::New | Op::NewSpread, false) => jit::JitDirectCallKind::Construct,
                    (Op::New | Op::NewSpread, true) => jit::JitDirectCallKind::DerivedConstruct,
                    (Op::SuperConstruct | Op::SuperConstructSpread, false) => {
                        jit::JitDirectCallKind::SuperConstruct
                    }
                    (Op::SuperConstruct | Op::SuperConstructSpread, true) => {
                        jit::JitDirectCallKind::DerivedSuperConstruct
                    }
                    _ => jit::JitDirectCallKind::Plain,
                };
                self.record_jit_direct_call_plan(
                    call_kind,
                    fid,
                    instruction_pc,
                    tier,
                    callee_fid,
                    target_index,
                    target_count,
                    direct_call_outcome,
                );
                if (is_construct
                    && (tier != jit_debug::JitDebugTier::Optimizing
                        || op != Op::New
                        || callee.is_derived_constructor
                        || callee.is_arrow))
                    || op == Op::CallSpread
                {
                    continue;
                }
                // Only the optimizing tier splices a reduced `f.call` target.
                let tier_splices =
                    !function_prototype_call || tier == jit_debug::JitDebugTier::Optimizing;
                if inline_ineligible
                    || target_count != 1
                    || !tier_splices
                    || !callee.admits_graph_inlining()
                {
                    continue;
                }
                let Some(body) = self.bake_inline_body(
                    &callee_context,
                    callee_fid,
                    tier,
                    budget,
                    Some(call_byte_pc),
                    view.call_frequencies.get(&call_byte_pc).copied(),
                    construct_receiver,
                ) else {
                    self.record_jit_inline_candidate(
                        fid,
                        instruction_pc,
                        tier,
                        Some(callee_fid),
                        Some(jit_debug::JitInlineRejectionReason::MissingSnapshot),
                    );
                    continue;
                };
                self.record_jit_inline_candidate(fid, instruction_pc, tier, Some(callee_fid), None);
                view.inline_callees
                    .insert(call_byte_pc, jit::JitInlineCallee { body });
            }
        }

        // Method-call sites: snapshot monomorphic and polymorphic feedback for
        // `fid` first so the per-target `shape_offset_of` (which needs
        // `&mut self`) does not alias the feedback map borrow. Each snapshot is a
        // list of candidate targets — one for `Mono`, up to
        // `MAX_POLY_METHOD_TARGETS` (most-frequent first) for `Poly`.
        // `Megamorphic` sites are skipped and side-exit before method lookup.
        //
        // A named load or store site's targets are the getters or setters its
        // receivers reached through an inherited accessor: the same guard, with
        // the holder slot holding the accessor's cell. Only the optimizing tier
        // calls them; nothing inlines them.
        struct PolySnapshot {
            instruction_pc: u32,
            call_byte_pc: u32,
            targets: SmallVec<[PolyMethodTarget; MAX_POLY_METHOD_TARGETS]>,
            accessor_byte: Option<u32>,
        }
        let method_sites: Vec<PolySnapshot> =
            view.instructions
                .iter()
                .filter_map(|instr| {
                    let accessor_byte = match instr.op(&view.code_block) {
                        Op::CallMethodValue => None,
                        Op::LoadProperty if tier == jit_debug::JitDebugTier::Optimizing => {
                            Some(crate::object::ACCESSOR_CELL_GETTER_BYTE)
                        }
                        Op::StoreProperty | Op::StorePropertyStrict
                            if tier == jit_debug::JitDebugTier::Optimizing =>
                        {
                            Some(crate::object::ACCESSOR_CELL_SETTER_BYTE)
                        }
                        _ => return None,
                    };
                    let site = instr.property_ic_site(&view.code_block)?;
                    let state = self.vm.method_target_feedback(site)?;
                    match state {
                        MethodCallFeedback::Mono {
                            method_fid,
                            recv_shape,
                            prototype,
                            method_field,
                        } => {
                            let mut targets: SmallVec<[PolyMethodTarget; MAX_POLY_METHOD_TARGETS]> =
                                SmallVec::new();
                            targets.push(PolyMethodTarget {
                                method_fid,
                                recv_shape,
                                prototype,
                                method_field,
                                hits: 1,
                            });
                            Some(PolySnapshot {
                                instruction_pc: instr.instruction_pc(&view.code_block),
                                call_byte_pc: instr.byte_pc,
                                targets,
                                accessor_byte,
                            })
                        }
                        MethodCallFeedback::Poly(observed) => {
                            let mut targets = (*observed).clone();
                            // Most-frequent target first: the common receiver shape
                            // then hits the shortest guard chain.
                            targets.sort_by_key(|t| std::cmp::Reverse(t.hits));
                            Some(PolySnapshot {
                                instruction_pc: instr.instruction_pc(&view.code_block),
                                call_byte_pc: instr.byte_pc,
                                targets,
                                accessor_byte,
                            })
                        }
                        // Native leaf sites are baked by their own pass: they
                        // carry an entry id rather than a callee body, so there is
                        // no inline chain to build here.
                        MethodCallFeedback::MonoNativeLeaf { .. }
                        | MethodCallFeedback::Megamorphic => None,
                    }
                })
                .collect();
        for snap in method_sites {
            let mut method_targets = Vec::with_capacity(snap.targets.len());
            let target_count = u32::try_from(snap.targets.len()).unwrap_or(u32::MAX);
            for (target_index, target) in snap.targets.iter().enumerate() {
                let (callee_function_id, outcome) = match self.bake_one_method_target(
                    context,
                    target,
                    u32::try_from(target_index).unwrap_or(u32::MAX),
                    target_count,
                    tier,
                    budget,
                    Some(snap.call_byte_pc),
                    view.call_frequencies.get(&snap.call_byte_pc).copied(),
                    snap.accessor_byte,
                ) {
                    Ok(method) => {
                        let outcome = method.callee.map_or(
                            jit_debug::JitDirectCallPlanOutcome::Rejected {
                                reason: jit_debug::JitDirectCallRejectionReason::NoEntryGeneration,
                            },
                            |callee| {
                                let plan = callee.plan;
                                jit_debug::JitDirectCallPlanOutcome::Available {
                                    code_object_id: plan.code_object_id,
                                    target_tier: match plan.tier {
                                        native_abi::NativeFrameKind::Baseline => {
                                            jit_debug::JitDebugTier::Template
                                        }
                                        native_abi::NativeFrameKind::Optimizing => {
                                            jit_debug::JitDebugTier::Optimizing
                                        }
                                        native_abi::NativeFrameKind::Interpreter
                                        | native_abi::NativeFrameKind::Host => {
                                            jit_debug::JitDebugTier::Interpreter
                                        }
                                    },
                                    this_mode: jit::JitDirectCallThisMode::MethodReceiver,
                                }
                            },
                        );
                        method_targets.push(method);
                        (target.method_fid, outcome)
                    }
                    Err(reason) => (
                        target.method_fid,
                        jit_debug::JitDirectCallPlanOutcome::Rejected { reason },
                    ),
                };
                self.record_jit_direct_call_plan(
                    jit::JitDirectCallKind::Method,
                    fid,
                    snap.instruction_pc,
                    tier,
                    callee_function_id,
                    u32::try_from(target_index).unwrap_or(u32::MAX),
                    target_count,
                    outcome,
                );
            }
            if !method_targets.is_empty() {
                view.method_targets
                    .insert(snap.call_byte_pc, method_targets);
            }
        }
    }

    /// Materialize one exact receiver/prototype/method-slot identity guard.
    ///
    /// Feedback keeps stable shape ids; generated code consumes compressed
    /// shape-handle offsets. Resolving every hop here keeps heap/runtime layout
    /// knowledge on the VM side.
    fn bake_method_guard(&mut self, target: &PolyMethodTarget) -> Option<jit::JitMethodGuard> {
        let recv_shape = self.bake_shape_id(target.recv_shape)?;
        let proven = self.proven_method(target);
        let (prototype_validity, holder_root) = match &target.prototype.validity {
            Some(cell) => (
                Some(self.bake_prototype_validity(cell)?),
                self.bake_shape_id(target.prototype.holder_root)?,
            ),
            None => (None, 0),
        };
        Some(jit::JitMethodGuard {
            method_fid: target.method_fid,
            recv_shape,
            prototype_validity,
            holder_root,
            method_field: target.method_field,
            slot_proven: proven.is_some(),
            method_cell: proven.map_or(0, |method| self.bake_held_identity_cell(method)),
            accessor_byte: None,
        })
    }

    /// The method `target`'s prototype proof fixes in its holder slot: the
    /// proof still holds, and the slot it names holds a closure of the
    /// target's function now. Every write to a watched prototype retires the
    /// proof, so the slot cannot change while generated code finds it valid.
    fn proven_method(&mut self, target: &PolyMethodTarget) -> Option<Value> {
        let cell = target.prototype.validity.as_ref()?;
        if !cell.is_valid() {
            return None;
        }
        let root = self.shape_for_id(target.prototype.holder_root)?;
        let crate::object::shape_body::ShapePrototype::Object(holder) =
            crate::object::shape_body::prototype_of(root)
        else {
            return None;
        };
        crate::object::load_slot_at_field(holder, &self.vm.gc_heap, target.method_field).filter(
            |value| {
                value
                    .as_closure(&self.vm.gc_heap)
                    .is_some_and(|closure| closure.function_id() == target.method_fid)
            },
        )
    }

    /// Bake the guard, optional call plan and one body of an exact method target.
    ///
    /// Each bounded mono/poly feedback target reaches this helper independently.
    /// An entry-capable generation enables Graph inlining and generated calls;
    /// Template field proofs can retain a body without that call plan. Every
    /// body is prepared once: a refused Graph frequency/budget admission is not
    /// retried as a second, frequency-free leaf request. Synthetic constructor
    /// targets have no source call PC and retain only generated-call admission.
    /// Accessors retain their guard and call plan without preparing a body.
    fn bake_one_method_target(
        &mut self,
        context: &ExecutionContext,
        target: &PolyMethodTarget,
        target_index: u32,
        target_count: u32,
        tier: jit_debug::JitDebugTier,
        budget: &mut InlineSnapshotBudget,
        call_byte_pc: Option<u32>,
        site_frequency: Option<f32>,
        accessor_byte: Option<u32>,
    ) -> Result<jit::JitMethodTarget, jit_debug::JitDirectCallRejectionReason> {
        let method_context = context
            .for_function(target.method_fid)
            .map_err(|_| jit_debug::JitDirectCallRejectionReason::MissingCallee)?;
        let method = method_context
            .exec_function(target.method_fid)
            .ok_or(jit_debug::JitDirectCallRejectionReason::MissingCallee)?;
        if !method.admits_generated_call(jit::JitDirectCallKind::Method) {
            return Err(jit_debug::JitDirectCallRejectionReason::IneligibleFunction);
        }
        let mut guard = self
            .bake_method_guard(target)
            .ok_or(jit_debug::JitDirectCallRejectionReason::MethodGuardUnavailable)?;
        if accessor_byte.is_some() {
            guard.accessor_byte = accessor_byte;
            guard.slot_proven = false;
            guard.method_cell = 0;
        }
        let callee = self.vm.current_direct_callee_plan(method).map(|mut plan| {
            debug_assert_eq!(plan.function_id, target.method_fid);
            plan.callee_cell = self.bake_callee_identity_cell();
            jit::JitDirectCallee {
                plan,
                receiver_allocation: None,
            }
        });
        let template_body = call_byte_pc.is_some()
            && accessor_byte.is_none()
            && !method.requires_argument_frame()
            && !method.makes_function;
        if callee.is_none() && !template_body {
            return Err(jit_debug::JitDirectCallRejectionReason::NoEntryGeneration);
        }
        let graph_body = tier == jit_debug::JitDebugTier::Optimizing
            && accessor_byte.is_none()
            && callee.is_some()
            && !method.requires_argument_frame()
            && method.admits_graph_inlining();
        let mut body = if graph_body || template_body {
            self.bake_inline_body(
                &method_context,
                target.method_fid,
                tier,
                budget,
                call_byte_pc,
                if graph_body { site_frequency } else { None },
                None,
            )
        } else {
            None
        };
        // A proven holder slot fixes the closure the body runs as, so the
        // body may read that closure's watched context slots as constants.
        if graph_body
            && let Some(closure) = self.proven_method(target)
            && let Some(context) = closure
                .as_closure(&self.vm.gc_heap)
                .map(|closure| closure.context(&self.vm.gc_heap))
            && let Some(body) = body.as_mut().and_then(std::sync::Arc::get_mut)
        {
            body.closure_constants = self.bake_closure_constants(context, &body.code_block);
            body.unique_closure_context = None;
        }
        let inline_fields = if template_body {
            body.as_deref()
                .and_then(|body| self.bake_method_inline_fields(&method_context, target, body))
        } else {
            None
        };
        if !graph_body && inline_fields.is_none() {
            body = None;
        }
        if callee.is_none() && body.is_none() {
            return Err(jit_debug::JitDirectCallRejectionReason::NoEntryGeneration);
        }
        Ok(jit::JitMethodTarget {
            target_index,
            target_count,
            guard,
            callee,
            body,
            inline_fields,
        })
    }

    fn bake_unique_closure_context(&mut self, view: &mut jit::JitCompileSnapshot) {
        let Some((_, context)) = self
            .vm
            .jit_code_registry
            .closure_contexts
            .single(view.code_block.id)
        else {
            return;
        };
        // The source fact observes a context weakly. Once a compilation
        // actually uses it, the generation must own the value its unguarded
        // LoadCellValue reads. Publish that strong root before preparing any
        // further assumptions; an unused context is removed from this session.
        let root_index = self.roots.len();
        let cell = self.bake_held_identity_cell(context);
        let constants = self.bake_closure_constants(context, &view.code_block);
        if !constants.is_empty() {
            view.unique_closure_context = Some(cell);
            view.closure_constants = constants;
        } else {
            self.roots.remove(root_index);
        }
    }

    /// The slots of `context`'s chain that `code` reads, each now
    /// watched, with the value it holds. A slot qualifies within the
    /// watchable range, never written since an earlier watch, at a
    /// coordinate the body itself never writes, and holding neither the
    /// hole nor `undefined` — a binding still awaiting its initialization,
    /// like one the body assigns, would only fire its watch.
    fn bake_closure_constants(
        &mut self,
        context: Value,
        code: &crate::executable::CodeBlock,
    ) -> Vec<jit::JitContextConstant> {
        use otter_bytecode::Op;
        let Some(base) = context.as_context() else {
            return Vec::new();
        };
        let mut seen: rustc_hash::FxHashSet<(u16, u16)> = code
            .code
            .iter()
            .filter_map(|instruction| {
                let position = match code.op(instruction) {
                    Op::StoreContextSlot
                    | Op::StoreContextSlotChecked
                    | Op::BindThisContextSlot => 2,
                    Op::StoreLookupSlot => 3,
                    _ => return None,
                };
                let Some(otter_bytecode::Operand::Imm32(word)) =
                    code.operand(instruction, position)
                else {
                    return None;
                };
                otter_bytecode::ContextCoord::from_imm32(word)
                    .map(|coord| (coord.depth, coord.slot))
            })
            .collect();
        let mut constants = Vec::new();
        for instruction in code.code.iter() {
            if !matches!(
                code.op(instruction),
                Op::LoadContextSlot | Op::LoadContextSlotChecked
            ) {
                continue;
            }
            let Some(otter_bytecode::Operand::Imm32(word)) = code.operand(instruction, 2) else {
                continue;
            };
            let Some(coord) = otter_bytecode::ContextCoord::from_imm32(word) else {
                continue;
            };
            if !seen.insert((coord.depth, coord.slot)) {
                continue;
            }
            let Some(context) = crate::context::walk(&self.vm.gc_heap, base, coord.depth) else {
                continue;
            };
            let Some(value) = crate::context::read_slot(&self.vm.gc_heap, context, coord.slot)
            else {
                continue;
            };
            if value.is_hole() || value.is_undefined() {
                continue;
            }
            // The isolate starts the slot's watch at installation, after
            // finding the slot still holds this value.
            let Some(watch) = self.vm.jit_code_registry.watchable_context_slot(
                &self.vm.gc_heap,
                context,
                coord.slot,
            ) else {
                continue;
            };
            let mut value_cell = None;
            let constant = if let Some(int) = value.as_i32() {
                jit::JitConstantValue::Int32(int)
            } else if value.is_cell() {
                let shape = value
                    .as_object()
                    .map(|object| crate::object::shape(object, &self.vm.gc_heap))
                    .filter(|&shape| crate::object::shape_body::is_stable(shape))
                    .and_then(|shape| self.bake_shape(shape))
                    .unwrap_or(0);
                let closure = value.as_closure(&self.vm.gc_heap);
                let bag = closure
                    .and_then(|closure| closure.own_props(&self.vm.gc_heap))
                    .map_or(0, |bag| self.bake_held_identity_cell(Value::object(bag)));
                let held = self.held_cell(value);
                let cell = held.address();
                value_cell = Some(held);
                jit::JitConstantValue::Cell {
                    cell,
                    object: value.is_object_type(),
                    shape,
                    function: closure.map(|closure| closure.cached_function_id),
                    bag,
                }
            } else {
                jit::JitConstantValue::Bits(value.to_bits())
            };
            let context = self.held_cell(Value::context(context));
            self.install.context_slot_watches.push(ContextSlotWatch {
                context,
                slot: coord.slot,
                watch,
                bits: value.to_bits(),
                value_cell,
            });
            constants.push(jit::JitContextConstant {
                depth: coord.depth,
                slot: coord.slot,
                watch,
                value: constant,
            });
        }
        constants
    }

    /// Bake the finalized actual new.target family observed at this source
    /// site. Unknown/provisional/changed/unsupported owners have no fit plan.
    fn bake_receiver_allocation_plan(
        &mut self,
        base_function_id: u32,
        family_id: u64,
    ) -> Option<(jit::JitReceiverAllocationPlan, ConstructReceiver)> {
        let (layout, new_target_function_id, new_target_is_class) = self
            .vm
            .finalized_constructor_layout_for_id(family_id, base_function_id)?;
        let (root_handle, prototype) = self
            .vm
            .gc_heap
            .read_payload(layout, |body| (body.root(), body.prototype()));
        // Generated code proves only the family identity; the family fixes an
        // ordinary prototype object, never an exotic one.
        if crate::object::shape_body::state_of(root_handle).is_provisional()
            || prototype.as_object().is_none()
        {
            return None;
        }
        let root_id = crate::object::shape_body::id_of(root_handle);
        let inline_capacity = crate::object::shape_body::inline_capacity_of(root_handle);
        if inline_capacity > crate::object::MAX_INLINE_CAPACITY {
            return None;
        }
        let prototype_root = self.bake_shape(root_handle)?;
        let cell = self
            .vm
            .gc_heap
            .read_payload(layout, |body| body.preparation_validity())?;
        let prototype_validity = self.bake_prototype_validity(&cell)?;
        // Receivers are born in the initializer's final shape only while the
        // family's current preparation covers every initializer field: a
        // name the chain gained since (an accessor, a read-only property)
        // ends the prepared prefix, and its store must run as written.
        let prepared = self
            .vm
            .gc_heap
            .read_payload(layout, |body| {
                body.prepared_capacity(root_handle, prototype)
            })
            .unwrap_or(0);
        let mut initial_shape = root_handle;
        let (receiver_shape, initial_field_count) = match (
            self.vm
                .simple_constructor_init_cache()
                .get(&base_function_id)
                .and_then(Option::as_ref)
                .map(|init| init.fields.len()),
            self.vm
                .simple_constructor_shape_cache()
                .get(&(base_function_id, root_id))
                .copied(),
        ) {
            (Some(field_count), Some(shape))
                if field_count <= inline_capacity
                    && field_count <= prepared
                    && !crate::object::shape_body::state_of(shape).is_provisional()
                    && crate::object::shape_body::inline_capacity_of(shape) == inline_capacity =>
            {
                debug_assert_eq!(
                    crate::object::shape_property_count(shape, &self.vm.gc_heap) as usize,
                    field_count,
                    "source initial fields and final shape diverged"
                );
                // Generated receivers hold undefined in these fields only
                // until the store-only body overwrites them, before anything
                // can read them: the bound records the stored values alone.
                initial_shape = shape;
                (self.bake_shape(shape)?, u8::try_from(field_count).ok()?)
            }
            _ => (0, 0),
        };
        self.roots
            .push(crate::jit_roots::CompilationRoot::Family(layout));
        let plan = jit::JitReceiverAllocationPlan {
            new_target_function_id,
            new_target_is_class,
            family_id,
            family_cell: layout.offset(),
            receiver_shape,
            initial_field_count,
            inline_capacity: u8::try_from(inline_capacity).ok()?,
            prototype_validity: Some(prototype_validity),
            prototype_root,
        };
        let receiver = ConstructReceiver {
            shape: self.found_shape(initial_shape),
        };
        Some((plan, receiver))
    }

    /// Read a monomorphic own-data case directly from the shared interpreter PIC.
    ///
    /// A site whose cache is still empty falls back to the compiler's
    /// TypeScript class annotation, which resolves against the live instance
    /// shape of the annotated class. Any recorded observation supersedes it.
    pub(super) fn monomorphic_own_property_feedback(
        &mut self,
        context: &ExecutionContext,
        code_block: &CodeBlock,
        instruction: &jit::JitInstructionMetadata,
    ) -> Option<(u32, crate::object::FieldLocation)> {
        let op = instruction.op(code_block);
        let kind = match op {
            Op::LoadProperty => crate::property_ic::PropertyIcKind::Load,
            Op::StoreProperty => crate::property_ic::PropertyIcKind::Store,
            _ => return None,
        };
        let instruction_pc = instruction.instruction_pc(code_block) as usize;
        match code_block
            .property_feedback_at(instruction_pc, kind)?
            .state()
        {
            crate::feedback::PropertyFeedbackState::MonomorphicOwnData { shape, slot } => {
                let baked = self.bake_shape(shape)?;
                Some((baked, crate::object::field_location(shape, u32::from(slot))))
            }
            crate::feedback::PropertyFeedbackState::Empty => {
                let slot = self.class_annotation_property_slot(context, code_block, instruction)?;
                Some(slot)
            }
            _ => None,
        }
    }

    /// Own-data slot a class-annotated property site is expected to hit.
    ///
    /// The annotation names a locally declared class; the interpreter's
    /// simple-constructor cache holds the instance shape that class builds, so
    /// the property resolves to a concrete shape and slot without the site
    /// having run. Unsound by construction — the caller must only use this
    /// where the site already guards the shape it was given.
    pub(super) fn class_annotation_property_slot(
        &mut self,
        context: &ExecutionContext,
        code_block: &CodeBlock,
        instruction: &jit::JitInstructionMetadata,
    ) -> Option<(u32, crate::object::FieldLocation)> {
        let name_operand = match instruction.op(code_block) {
            Op::LoadProperty => 2,
            Op::StoreProperty => 1,
            _ => return None,
        };
        let class_fid = code_block.class_hint(instruction.instruction_pc(code_block) as usize)?;
        // One prototype lineage per class in practice; several leave the
        // site to its own feedback.
        let mut shapes = self
            .vm
            .simple_constructor_shape_cache()
            .iter()
            .filter(|((id, _), _)| *id == class_fid)
            .map(|(_, shape)| *shape);
        let shape = shapes.next()?;
        if shapes.next().is_some() {
            return None;
        }
        let otter_bytecode::Operand::ConstIndex(name_idx) =
            instruction.operand(code_block, name_operand)?
        else {
            return None;
        };
        let key = context.property_atom(name_idx)?;
        let slot = crate::object::shape_offset_of_str(&self.vm.gc_heap, shape, key.name())?;
        Some((
            self.bake_shape(shape)?,
            crate::object::field_location(shape, slot),
        ))
    }
}

impl crate::jit_roots::CompileProofs for Baker<'_> {
    fn shape(&mut self, shape: crate::object::ShapeHandle) -> Option<u32> {
        self.bake_shape(shape)
    }

    fn validity(
        &mut self,
        cell: &std::sync::Arc<crate::object::prototype_validity::PrototypeValidity>,
    ) -> Option<jit::JitPrototypeValidity> {
        self.bake_prototype_validity(cell)
    }
}

/// Mutator-side work one compilation asks for, run on the isolate thread
/// before its code is published. Every task re-verifies the fact the baked
/// code relies on; a task that fails declines the installation.
#[derive(Default)]
pub(crate) struct InstallTasks {
    /// `instanceof` cells the generation's sites own, registered so a change
    /// to a cached constructor empties them.
    pub(crate) instanceof_cells: Vec<std::sync::Arc<crate::jit_roots::InstanceofCell>>,
    /// Dictionary slots of the global object the code reads directly.
    pub(crate) global_slot_watches: Vec<GlobalSlotWatch>,
    /// Context slots the code reads as constants.
    pub(crate) context_slot_watches: Vec<ContextSlotWatch>,
}

/// A dictionary slot of the global object generated code reads under the
/// slot-layout epoch it was baked against. Installing watches the slot, so a
/// redefinition advances the layout the code guards; it declines when the
/// layout moved on since baking or the slot no longer holds data.
pub(crate) struct GlobalSlotWatch {
    pub(crate) slot: u16,
    pub(crate) layout: u32,
}

/// A context slot generated code reads as the constant it held when baked.
/// Installing subscribes the slot's watch, after re-reading the slot through
/// the traced context cell and finding the same value; the watch cell the
/// code tests is the one installation returns.
pub(crate) struct ContextSlotWatch {
    /// Holds the context; the collector rewrites it.
    pub(crate) context: std::sync::Arc<crate::jit_roots::CalleeIdentityCell>,
    pub(crate) slot: u16,
    /// The context's watch, which the code's dependency names.
    pub(crate) watch: u32,
    /// The value's bits when baked, unless the value is a heap cell, whose
    /// identity `value_cell` holds.
    pub(crate) bits: u64,
    pub(crate) value_cell: Option<std::sync::Arc<crate::jit_roots::CalleeIdentityCell>>,
}

/// Admit a guarded method call only when its declared entry has machine-callable
/// code, reserving the frame-slot root window an allocating entry needs.
///
/// The family the entry id resolves in is the whole decision: a leaf or
/// mutating-leaf entry cannot collect and publishes nothing, while an
/// allocating one must name a precise map covering the caller's full register
/// window before generated code may call it. A grow entry without callable
/// code is dropped, leaving the declared entry's misses on the ordinary call.
fn reserve_guarded_entry_safepoint(
    view: &mut jit::JitCompileSnapshot,
    call: &mut jit::JitGuardedMethodCall,
) -> bool {
    if !reserve_entry_safepoint(view, call.entry_stub_id, call.safepoint_id) {
        return false;
    }
    if let Some((stub_id, safepoint_id)) = call.grow
        && !reserve_entry_safepoint(view, stub_id, safepoint_id)
    {
        call.grow = None;
    }
    true
}

fn reserve_entry_safepoint(
    view: &mut jit::JitCompileSnapshot,
    stub_id: native_abi::RuntimeStubId,
    safepoint_id: native_abi::SafepointId,
) -> bool {
    if safepoint_id == native_abi::NO_SAFEPOINT {
        return crate::runtime_stubs::leaf_no_alloc_stub2_by_id(stub_id)
            .is_some_and(crate::runtime_stubs::LeafNoAllocStub2::is_valid)
            || crate::runtime_stubs::mutating_leaf_stub2_by_id(stub_id)
                .is_some_and(crate::runtime_stubs::MutatingLeafStub2::is_valid)
            || crate::runtime_stubs::mutating_leaf_stub3_by_id(stub_id)
                .is_some_and(crate::runtime_stubs::MutatingLeafStub3::is_valid);
    }
    if !crate::runtime_stubs::alloc_value_stub_by_id(stub_id)
        .is_some_and(|stub| stub.is_valid_for_safepoint(safepoint_id) && stub.has_entry())
    {
        return false;
    }
    view.safepoints.insert(
        safepoint_id,
        native_abi::SafepointRecord::window(safepoint_id, native_abi::NO_FRAME_STATE),
    );
    true
}
