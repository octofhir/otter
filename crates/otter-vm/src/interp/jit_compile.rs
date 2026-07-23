//! JIT compile requests and cold profile-feedback baking.
//!
//! # Contents
//! - `jit_code_residency` and `jit_code_generation_snapshot` — opt-in
//!   whole-isolate executable ownership and generation snapshots.
//! - `compile_jit_function` and cold feedback baking into the instruction view
//!   (property/object-literal/global-lexical/inline-callee tables).
//! - One-level call-graph tiering for hot observed callees that need an entry
//!   generation before the caller's stable direct-link snapshot is sealed.
//! - Call/method target profiling and reoptimization eviction.
//!
//! # Invariants
//! Baked pointers (shape ids, global cells, prototype slots) must only
//! reference permanent or non-moving allocations; anything movable goes
//! through a runtime stub instead.
//! Compiled code is published only after the registry accepts its metadata and
//! exact isolate-epoch dependency snapshot.
//! Eager direct-target compilation is bounded to one edge level; recursively
//! compiling an observed call graph is forbidden.
#![allow(unused_imports)]
use crate::*;

impl Interpreter {
    /// Snapshot installed, invalid, and retired-tombstone JIT generations.
    ///
    /// This explicit diagnostics call walks cold registry metadata and performs
    /// no work during ordinary compilation or execution.
    #[must_use]
    pub fn jit_code_generation_snapshot(&self) -> Vec<jit::JitCodeGenerationSnapshot> {
        self.jit_code_registry.generation_snapshot()
    }

    /// Snapshot all executable code objects currently retained by this isolate.
    ///
    /// This walks cold JIT ownership/cache tables only when explicitly called;
    /// ordinary dispatch, compilation, and stats collection do no residency
    /// accounting.
    #[must_use]
    pub fn jit_code_residency(&self) -> jit::JitCodeResidency {
        let mut seen = rustc_hash::FxHashSet::default();
        let mut code_bytes = 0u64;
        let mut record = |code: &std::sync::Arc<dyn jit::JitFunctionCode>| {
            let identity = std::sync::Arc::as_ptr(code) as *const () as usize;
            if seen.insert(identity) {
                code_bytes =
                    code_bytes.saturating_add(u64::try_from(code.code_len()).unwrap_or(u64::MAX));
            }
        };

        for code in self.jit_code.values().flatten() {
            record(code);
        }
        for code in self.jit_optimized_code.values().flatten() {
            record(code);
        }
        for code in self.jit_osr_code.values().flatten() {
            record(code);
        }
        if let Some((_, code)) = &self.jit_code_cache {
            record(code);
        }
        if let Some((_, code)) = &self.jit_optimized_code_cache {
            record(code);
        }
        jit::JitCodeResidency {
            installed_optimized_bodies: self.jit_optimized_code.values().flatten().count() as u64,
            installed_entry_bodies: self.jit_code.values().flatten().count() as u64,
            installed_osr_bodies: self.jit_osr_code.values().flatten().count() as u64,
            unique_code_objects: seen.len() as u64,
            code_bytes,
        }
    }

    fn record_jit_compile_prepared(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
        tier: jit_debug::JitDebugTier,
        target: jit_debug::JitDebugTarget,
        view: &jit::JitCompileSnapshot,
    ) {
        if !self.reserve_jit_debug_event() {
            return;
        }
        let function_name = context
            .function(fid)
            .map(|function| function.name.clone())
            .unwrap_or_else(|| "<unknown>".to_string());
        let method_feedback_sites = view
            .instructions
            .iter()
            .filter(|instr| {
                instr
                    .property_ic_site(&view.code_block)
                    .is_some_and(|site| self.method_target_feedback(site).is_some())
            })
            .count();
        let call_feedback_sites = view
            .instructions
            .iter()
            .filter(|instr| {
                view.code_block
                    .call_distribution_at(instr.instruction_pc(&view.code_block) as usize)
                    .is_some()
            })
            .count();
        let global_load_sites = view
            .instructions
            .iter()
            .filter(|instruction| instruction.op(&view.code_block) == Op::LoadGlobalOrThrow)
            .count();
        let event = jit_debug::JitDebugEvent::CompilePrepared {
            function_id: fid,
            function_name,
            tier,
            target,
            register_count: u32::from(view.code_block.register_count),
            parameter_count: u32::from(view.code_block.param_count),
            call_feedback_sites: u32::try_from(call_feedback_sites).unwrap_or(u32::MAX),
            method_feedback_sites: u32::try_from(method_feedback_sites).unwrap_or(u32::MAX),
            global_load_sites: u32::try_from(global_load_sites).unwrap_or(u32::MAX),
            global_lexical_loads: u32::try_from(view.global_lexical_loads.len())
                .unwrap_or(u32::MAX),
            global_object_loads: u32::try_from(view.global_object_loads.len()).unwrap_or(u32::MAX),
            direct_callees: u32::try_from(view.direct_callees.len()).unwrap_or(u32::MAX),
            direct_method_sites: u32::try_from(view.direct_methods.len()).unwrap_or(u32::MAX),
            direct_method_targets: u32::try_from(
                view.direct_methods.values().map(Vec::len).sum::<usize>(),
            )
            .unwrap_or(u32::MAX),
            static_native_calls: u32::try_from(view.static_native_calls.len()).unwrap_or(u32::MAX),
            inline_callees: u32::try_from(view.inline_callees.len()).unwrap_or(u32::MAX),
            inline_methods: u32::try_from(view.inline_methods.len()).unwrap_or(u32::MAX),
        };
        self.push_reserved_jit_debug_event(event);
    }

    fn record_jit_compile_finished(
        &mut self,
        fid: u32,
        tier: jit_debug::JitDebugTier,
        target: jit_debug::JitDebugTarget,
        code_object_id: u64,
        status: &Result<jit::JitCompileStatus, jit::JitCompileError>,
    ) {
        if !self.reserve_jit_debug_event() {
            return;
        }
        let outcome = match status {
            Ok(jit::JitCompileStatus::Compiled { code, .. }) => {
                jit_debug::JitDebugCompileOutcome::Compiled {
                    code_object_id,
                    code_bytes: u64::try_from(code.code_len()).unwrap_or(u64::MAX),
                }
            }
            Ok(jit::JitCompileStatus::Unavailable) => {
                jit_debug::JitDebugCompileOutcome::Unavailable
            }
            Ok(jit::JitCompileStatus::Unsupported { reason }) => {
                jit_debug::JitDebugCompileOutcome::Unsupported {
                    reason: reason.clone(),
                }
            }
            Err(error) => jit_debug::JitDebugCompileOutcome::Error {
                message: error.message.clone(),
            },
        };
        self.push_reserved_jit_debug_event(jit_debug::JitDebugEvent::CompileFinished {
            function_id: fid,
            tier,
            target,
            outcome,
        });
        let Ok(jit::JitCompileStatus::Compiled { diagnostics, .. }) = status else {
            return;
        };
        for diagnostic in diagnostics {
            if !self.reserve_jit_debug_event() {
                break;
            }
            let event = match *diagnostic {
                jit_debug::JitCompilerDiagnostic::DirectCallLowered {
                    call_kind,
                    instruction_pc,
                    byte_pc,
                    callee_function_id,
                    target_index,
                    target_count,
                    outcome,
                } => jit_debug::JitDebugEvent::DirectCallLowered {
                    call_kind,
                    caller_function_id: fid,
                    caller_code_object_id: code_object_id,
                    instruction_pc,
                    byte_pc,
                    tier,
                    callee_function_id,
                    target_index,
                    target_count,
                    outcome,
                },
                jit_debug::JitCompilerDiagnostic::StaticNativeCallLowered {
                    instruction_pc,
                    byte_pc,
                    target,
                    outcome,
                } => jit_debug::JitDebugEvent::StaticNativeCallLowered {
                    caller_function_id: fid,
                    caller_code_object_id: code_object_id,
                    instruction_pc,
                    byte_pc,
                    tier,
                    target,
                    outcome,
                },
            };
            self.push_reserved_jit_debug_event(event);
        }
    }

    fn record_jit_inline_candidate(
        &mut self,
        caller_function_id: u32,
        instruction_pc: u32,
        tier: jit_debug::JitDebugTier,
        callee_function_id: Option<u32>,
        bake_rejection: Option<jit_debug::JitInlineRejectionReason>,
    ) {
        self.record_jit_debug_event(|| jit_debug::JitDebugEvent::InlineCandidate {
            caller_function_id,
            instruction_pc,
            tier,
            callee_function_id,
            bake_rejection,
        });
    }

    fn record_jit_direct_call_plan(
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
        self.record_jit_debug_event(|| jit_debug::JitDebugEvent::DirectCallPlan {
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

    fn record_jit_static_native_call_plan(
        &mut self,
        caller_function_id: u32,
        instruction_pc: u32,
        tier: jit_debug::JitDebugTier,
        target: jit::JitStaticNativeCallKind,
    ) {
        self.record_jit_debug_event(|| jit_debug::JitDebugEvent::StaticNativeCallPlan {
            caller_function_id,
            instruction_pc,
            tier,
            target,
        });
    }

    /// Replace `fid`'s current native generation with one optimizing-tier body
    /// compiled from the latest feedback snapshot.
    ///
    /// Compilation leaves the current baseline generation installed. A
    /// successful entry-capable optimizer is published through the function's
    /// stable entry cell; generated callers observe it on their next entry
    /// without recompilation. A declined optimizer leaves the baseline target
    /// untouched.
    ///
    /// Unsupported functions return `None`; hotness survives invalidation, so
    /// the normal resolver can immediately rebuild a baseline body.
    pub(crate) fn compile_optimized_jit_function(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
        osr_pc: Option<u32>,
    ) -> Option<std::sync::Arc<dyn jit::JitFunctionCode>> {
        let hook = self.jit_hook.as_ref()?.clone();
        if !hook.optimizing_tier_enabled() {
            return None;
        }
        let mut snapshot = context.jit_compile_snapshot(fid)?;
        self.publish_property_feedback_for_view(&snapshot);
        // The optimizing tier consumes the same baked compile inputs as the
        // template tier: without the cage base and body offsets no inline access
        // can be emitted at all, and without monomorphic call-site candidates
        // there is nothing to inline.
        Self::bake_typed_array_layout(&mut snapshot);
        Self::bake_string_layout(&mut snapshot);
        self.bake_global_lexical_loads(&mut snapshot, context, fid);
        self.bake_inline_callees(
            &mut snapshot,
            context,
            fid,
            jit_debug::JitDebugTier::Optimizing,
            false,
        );
        self.bake_collection_leaf_methods(&mut snapshot);
        self.bake_primitive_method_guards(&mut snapshot);
        let target = osr_pc.map_or(jit_debug::JitDebugTarget::Entry, |pc| {
            jit_debug::JitDebugTarget::Osr { pc }
        });
        self.record_jit_compile_prepared(
            context,
            fid,
            jit_debug::JitDebugTier::Optimizing,
            target,
            &snapshot,
        );
        let artifact_identity =
            self.jit_debug
                .request()
                .artifacts_enabled()
                .then(|| crate::JitArtifactIdentity {
                    function_name: context
                        .function(fid)
                        .map(|function| function.name.clone())
                        .unwrap_or_else(|| "<unknown>".to_string()),
                    module: snapshot.code_block.module_url().to_string(),
                });
        let function = snapshot.code_block.clone();
        let code_object_id = self.jit_next_code_object_id;
        let status = hook.compile_optimized_function(jit::JitCompileRequest {
            snapshot,
            debug: self.jit_debug.request(),
            artifact_identity,
            osr_pc,
            code_object_id,
        });
        self.record_jit_compile_finished(
            fid,
            jit_debug::JitDebugTier::Optimizing,
            target,
            code_object_id,
            &status,
        );
        match status {
            Ok(jit::JitCompileStatus::Compiled { code, artifact, .. }) => {
                if let Some(artifact) = artifact {
                    self.record_jit_artifact(*artifact);
                }
                self.jit_next_code_object_id += 1;
                // Generated callers do not take a per-call executable lease.
                // Any published native activation pins the current retirement
                // epoch, including across reentrant compilation/invalidation.
                if self.jit_native_activation_top == 0 {
                    self.jit_code_registry.retire_unreferenced();
                }
                let installed = self.jit_code_registry.install_compiled(
                    code_object_id,
                    code.clone(),
                    &function,
                );
                if installed {
                    self.jit_runtime_stats.code_generations =
                        self.jit_runtime_stats.code_generations.saturating_add(1);
                }
                installed.then_some(code)
            }
            _ => None,
        }
    }

    /// Resolve or compile one whole-body optimizer object for loop OSR.
    ///
    /// Back-edge hotness is independent of function-entry promotion: a
    /// single-call loop can therefore compile here before the entry policy is
    /// hot. Successful code is shared with later function entries. A failed
    /// early feedback snapshot is not cached as a permanent entry-tier miss;
    /// the current header falls back to template OSR and future entry feedback
    /// may still make the function optimizable.
    pub(crate) fn resolve_optimized_osr_code(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
        osr_pc: u32,
    ) -> Option<std::sync::Arc<dyn jit::JitFunctionCode>> {
        if !self
            .jit_hook
            .as_ref()
            .is_some_and(|hook| hook.optimizing_tier_enabled())
        {
            return None;
        }
        if let Some(Some(code)) = self.jit_optimized_code.get(&fid) {
            return self
                .jit_code_registry
                .is_current_for_entry(code.as_ref())
                .then(|| code.clone());
        }
        // A declined body is retried at a back-edge only when its feedback epoch
        // has advanced since the last failed attempt: the optimizer's whole-body
        // subset check is structural (unsupported opcode) except for
        // feedback-driven representation, so re-running it while the feedback is
        // unchanged always fails again and would recompile on every hot iteration.
        let epoch = self.code_space.feedback_epoch(fid);
        if matches!(self.jit_optimized_code.get(&fid), Some(None))
            && self.jit_optimized_declined_epoch.get(&fid) == Some(&epoch)
        {
            return None;
        }
        match self.compile_optimized_jit_function(context, fid, Some(osr_pc)) {
            Some(compiled) => {
                self.jit_optimized_code.insert(fid, Some(compiled.clone()));
                self.jit_optimized_declined_epoch.remove(&fid);
                self.jit_optimized_code_cache = Some((fid, compiled.clone()));
                Some(compiled)
            }
            None => {
                self.jit_optimized_code.insert(fid, None);
                self.jit_optimized_declined_epoch.insert(fid, epoch);
                None
            }
        }
    }

    /// Build a compile request for `fid` and run the installed hook. Returns the
    /// installed code, or `None` when the hook declines (unsupported subset or
    /// executable memory unavailable) — either way execution stays correct on
    /// the interpreter.
    pub(crate) fn compile_jit_function(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
        osr_pc: Option<u32>,
    ) -> Option<std::sync::Arc<dyn jit::JitFunctionCode>> {
        self.compile_jit_function_with_direct_targets(context, fid, osr_pc, true)
    }

    /// Compile one template body, optionally materializing one level of hot
    /// direct-target entry generations before the caller snapshot is sealed.
    fn compile_jit_function_with_direct_targets(
        &mut self,
        context: &ExecutionContext,
        fid: u32,
        osr_pc: Option<u32>,
        eager_direct_targets: bool,
    ) -> Option<std::sync::Arc<dyn jit::JitFunctionCode>> {
        let mut view = context.jit_compile_snapshot(fid)?;
        self.publish_property_feedback_for_view(&view);
        Self::bake_typed_array_layout(&mut view);
        Self::bake_string_layout(&mut view);
        self.bake_global_lexical_loads(&mut view, context, fid);
        self.bake_inline_callees(
            &mut view,
            context,
            fid,
            jit_debug::JitDebugTier::Template,
            eager_direct_targets,
        );
        self.bake_collection_leaf_methods(&mut view);
        self.bake_collection_alloc_methods(&mut view);
        self.bake_array_methods(&mut view);
        self.bake_primitive_method_guards(&mut view);
        let target = osr_pc.map_or(jit_debug::JitDebugTarget::Entry, |pc| {
            jit_debug::JitDebugTarget::Osr { pc }
        });
        self.record_jit_compile_prepared(
            context,
            fid,
            jit_debug::JitDebugTier::Template,
            target,
            &view,
        );
        let function = view.code_block.clone();
        let hook = self.jit_hook.as_ref()?.clone();
        let code_object_id = self.jit_next_code_object_id;
        let artifact_identity =
            self.jit_debug
                .request()
                .artifacts_enabled()
                .then(|| crate::JitArtifactIdentity {
                    function_name: context
                        .function(fid)
                        .map(|function| function.name.clone())
                        .unwrap_or_else(|| "<unknown>".to_string()),
                    module: view.code_block.module_url().to_string(),
                });
        let status = hook.compile_function(jit::JitCompileRequest {
            snapshot: view,
            debug: self.jit_debug.request(),
            artifact_identity,
            osr_pc,
            code_object_id,
        });
        self.record_jit_compile_finished(
            fid,
            jit_debug::JitDebugTier::Template,
            target,
            code_object_id,
            &status,
        );
        match status {
            Ok(jit::JitCompileStatus::Compiled { code, artifact, .. }) => {
                if let Some(artifact) = artifact {
                    self.record_jit_artifact(*artifact);
                }
                self.jit_next_code_object_id += 1;
                // Sweep before registering: cached/installed users hold an
                // `Arc`; executing generated generations are protected by the
                // isolate's published native-activation epoch.
                if self.jit_native_activation_top == 0 {
                    self.jit_code_registry.retire_unreferenced();
                }
                let installed = self.jit_code_registry.install_compiled(
                    code_object_id,
                    code.clone(),
                    &function,
                );
                if installed {
                    self.jit_runtime_stats.code_generations =
                        self.jit_runtime_stats.code_generations.saturating_add(1);
                }
                installed.then_some(code)
            }
            _ => None,
        }
    }

    /// Bake fixed Array-body fields used by native guards. Element backing
    /// stores remain behind runtime stubs and are deliberately absent here.
    pub(crate) fn bake_typed_array_layout(view: &mut jit::JitCompileSnapshot) {
        let header = otter_gc::header::HEADER_SIZE as u32;
        view.array_layout = jit::JitArrayLayout {
            type_tag: crate::array::ARRAY_BODY_TYPE_TAG,
            length_byte: header + crate::array::ARRAY_BODY_LENGTH_OFFSET as u32,
            exotic_byte: header + std::mem::offset_of!(crate::array::ArrayBody, exotic) as u32,
            elements_ptr_byte: header + crate::array::ARRAY_BODY_ELEMENTS_PTR_OFFSET as u32,
            dense_len_byte: header + crate::array::ARRAY_BODY_DENSE_LEN_OFFSET as u32,
        };
        view.cage_base = otter_gc::cage_base() as usize;
    }

    /// Bake the static heap-layout offsets for inline primitive string fast
    /// paths. String bodies are GC cells addressed through the same cage base as
    /// object/array bodies, so this only enables when the compile snapshot has a
    /// cage base.
    pub(crate) fn bake_string_layout(view: &mut jit::JitCompileSnapshot) {
        let header = otter_gc::header::HEADER_SIZE as u32;
        view.string_layout = jit::JitStringLayout {
            string_type_tag: crate::string::JS_STRING_BODY_TYPE_TAG,
            string_len_byte: header + std::mem::offset_of!(crate::string::JsStringBody, len) as u32,
        };
        view.cage_base = otter_gc::cage_base() as usize;
    }

    /// Read a monomorphic own-data case directly from the shared interpreter PIC.
    fn monomorphic_own_property_feedback(&self, op: Op, site: usize) -> Option<(u32, u32)> {
        const SLOT_BYTES: u32 =
            std::mem::size_of::<crate::value::compressed::CompressedValue>() as u32;
        let kind = match op {
            Op::LoadProperty => crate::property_ic::PropertyIcKind::Load,
            Op::StoreProperty => crate::property_ic::PropertyIcKind::Store,
            _ => return None,
        };
        self.publish_property_feedback(site, kind);
        let crate::feedback::PropertyFeedbackState::MonomorphicOwnData { shape_id, slot } =
            self.property_feedback_state(site, kind)?
        else {
            return None;
        };
        let shape = self.shape_runtime.handle_for_id(shape_id)?;
        Some((shape.offset(), u32::from(slot) * SLOT_BYTES))
    }

    /// Whether a method-call site's feedback has already saturated to
    /// `Megamorphic`. Once it has, further [`Self::note_method_target`]
    /// observations are no-ops, so a caller can skip the receiver/prototype
    /// shape walk that only exists to build the `MethodSite` argument — the hot
    /// path for a megamorphic site (e.g. one `arr[i].run()` over many classes).
    pub(crate) fn method_site_feedback_saturated(&self, site: usize) -> bool {
        self.method_target_feedback_saturated(site)
    }

    /// Record the live `Mono`/`Poly` overlay for one `Op::CallMethodValue` site.
    ///
    /// Method feedback does not invalidate installed machine code. Baseline
    /// method sites always retain their runtime IC/direct-link fallback, so new
    /// receiver shapes can populate that live overlay without rebuilding the
    /// caller's immutable executable body. The initial snapshot may still bake
    /// already-observed tiny method bodies; later observations use the live
    /// multi-entry link table until normal bail-driven tier policy decides that
    /// a fresh compilation is warranted.
    pub(crate) fn note_method_target(
        &mut self,
        feedback_site: usize,
        method_fid: u32,
        site: MethodSite,
    ) {
        self.record_method_target_feedback(feedback_site, method_fid, site);
    }

    pub(crate) fn method_site_for_receiver(
        &mut self,
        context: &ExecutionContext,
        caller_fid: u32,
        name_idx: u32,
        recv: Value,
    ) -> Option<MethodSite> {
        let name = context.property_atom_for_function(caller_fid, name_idx)?;
        let recv = recv.as_object()?;
        let recv_shape_handle = crate::object::shape(recv, &self.gc_heap);
        if recv_shape_handle.is_null() {
            return None;
        }
        let recv_shape = crate::object::shape_id(recv, &self.gc_heap);
        let slot_byte = |slot: u32| {
            slot * std::mem::size_of::<crate::value::compressed::CompressedValue>() as u32
        };
        if let Some(slot) = self.shape_offset_of(recv_shape_handle, name.name()) {
            return Some(MethodSite {
                recv_shape,
                proto_chain: crate::MethodProtoChain::own(),
                method_value_byte: slot_byte(slot),
            });
        }
        // Walk the prototype chain, recording each hopped object's shape; the
        // baked guard checks exactly this chain (flat-prototype chase + shape
        // compare per hop) before trusting the holder's slot offset.
        let mut proto_chain = crate::MethodProtoChain::own();
        let mut cur = recv;
        loop {
            cur = crate::object::prototype(cur, &self.gc_heap)?;
            let shape = crate::object::shape(cur, &self.gc_heap);
            if shape.is_null() || !proto_chain.push(crate::object::shape_id(cur, &self.gc_heap)) {
                return None;
            }
            if let Some(slot) = self.shape_offset_of(shape, name.name()) {
                return Some(MethodSite {
                    recv_shape,
                    proto_chain,
                    method_value_byte: slot_byte(slot),
                });
            }
        }
    }

    /// Drop any compiled body for `fid` (and re-arm its OSR headers) so the next
    /// tier-up recompiles it. Called when call/method feedback for one of its
    /// sites first matures: a function whose hot loop calls out is often compiled
    /// by an *earlier* loop in the same body, before the callee feedback exists,
    /// so its inline sites baked nothing. Recompiling once the feedback is warm
    /// lets those sites inline. The currently-running body, if any, stays alive
    /// through its `Arc` until the frame returns.
    pub(crate) fn evict_compiled_for_reopt(&mut self, fid: u32) {
        self.invalidate_jit_function(fid);
    }

    /// Resolve the stable entry cell for one compiler-native call.
    ///
    /// The registry publishes an optimizing generation only when it advertises
    /// safe stack-owned cold deoptimization; otherwise the current baseline
    /// remains selected. The returned function-cell address survives every
    /// later generation replacement.
    pub(crate) fn current_direct_callee_plan(
        &self,
        function: &CodeBlock,
    ) -> Option<jit::JitDirectCallPlan> {
        self.jit_code_registry.direct_call_plan(function)
    }

    /// Ensure one hot feedback target has an entry-capable baseline generation.
    ///
    /// The caller compile may run before a loop-heavy callee reaches the
    /// function-entry threshold: that callee can already own optimizing OSR
    /// code while every call boundary still returns to Rust. Compile at most
    /// this one target level, then seal the caller against its stable function
    /// cell. Nested eager planning is disabled in the target compile to keep
    /// compile work bounded by the caller's observed edge set.
    fn ensure_direct_callee_plan(
        &mut self,
        context: &ExecutionContext,
        function: &CodeBlock,
        eager: bool,
    ) -> Option<jit::JitDirectCallPlan> {
        if let Some(plan) = self.current_direct_callee_plan(function) {
            return Some(plan);
        }
        if !eager || self.jit_code.contains_key(&function.id) {
            return None;
        }
        self.jit_runtime_stats.compile_attempts =
            self.jit_runtime_stats.compile_attempts.saturating_add(1);
        let compiled =
            self.compile_jit_function_with_direct_targets(context, function.id, None, false);
        self.jit_code.insert(function.id, compiled.clone());
        self.jit_code_cache = None;
        if compiled.is_some() {
            self.jit_entry_osr_only.remove(&function.id);
        }
        self.current_direct_callee_plan(function)
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
    /// and global-object shape, so later eval/script lexicals and structural
    /// mutations miss before reading the baked slot.
    fn bake_global_lexical_loads(
        &self,
        view: &mut jit::JitCompileSnapshot,
        context: &ExecutionContext,
        fid: u32,
    ) {
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
            if let Some(&(cell, _)) = self.global_lexicals.get(name) {
                view.global_lexical_loads.insert(
                    instruction.byte_pc,
                    jit::JitGlobalLexicalLoad {
                        cell_offset: cell.offset(),
                    },
                );
                continue;
            }
            let (Some(hit), crate::object::PropertyLookup::Data { .. }) =
                crate::object::lookup_own_slot(self.global_this, &self.gc_heap, name)
            else {
                continue;
            };
            let shape = crate::object::shape(self.global_this, &self.gc_heap);
            let (shape, dictionary) = if shape.is_null() {
                (hit.shape_id.raw(), true)
            } else {
                (u64::from(shape.offset()), false)
            };
            const SLOT_BYTES: u32 =
                std::mem::size_of::<crate::value::compressed::CompressedValue>() as u32;
            view.global_object_loads.insert(
                instruction.byte_pc,
                jit::JitGlobalObjectLoad {
                    shape,
                    dictionary,
                    value_byte: u32::from(hit.slot) * SLOT_BYTES,
                    global_lexical_epoch: self.global_lexical_epoch,
                },
            );
        }
    }

    /// Bake compiler-native direct-call plans and inline-candidate bodies for
    /// `fid`'s call sites.
    ///
    /// Plain-call candidates remain monomorphic. Method-call candidates may
    /// contain a bounded, most-frequent-first polymorphic chain. Every generated
    /// target is a synchronous bytecode function without callee-owned upvalue
    /// cells and has one current non-OSR installed entry. Generated linkage binds
    /// both strict/lexical and unbound sloppy-global `this`; an explicitly bound
    /// sloppy closure misses before entry. The emitter applies the final
    /// pure-leaf / size / arity test to the separate monomorphic inline tables.
    pub(crate) fn bake_inline_callees(
        &mut self,
        view: &mut jit::JitCompileSnapshot,
        context: &ExecutionContext,
        fid: u32,
        tier: jit_debug::JitDebugTier,
        eager_direct_targets: bool,
    ) {
        let mut pending_direct_targets = rustc_hash::FxHashSet::default();
        let call_sites: Vec<_> = view
            .instructions
            .iter()
            .filter_map(|instr| {
                let instruction_pc = instr.instruction_pc(&view.code_block);
                let state = view
                    .code_block
                    .call_distribution_at(instruction_pc as usize)?;
                Some((instruction_pc, instr.byte_pc, state))
            })
            .collect();
        for (instruction_pc, call_byte_pc, state) in call_sites {
            let feedback::CallSiteDistribution::Mono(target) = state else {
                self.record_jit_inline_candidate(
                    fid,
                    instruction_pc,
                    tier,
                    None,
                    Some(jit_debug::JitInlineRejectionReason::Polymorphic),
                );
                continue;
            };
            let callee_fid = match target.target {
                feedback::OrdinaryCallTarget::Bytecode(callee_fid) => callee_fid,
                feedback::OrdinaryCallTarget::StaticNative(kind) => {
                    view.static_native_calls.insert(
                        call_byte_pc,
                        jit::JitStaticNativeCall {
                            kind,
                            builtin_fn_addr: crate::math::jit_static_call_address(kind),
                        },
                    );
                    self.record_jit_inline_candidate(
                        fid,
                        instruction_pc,
                        tier,
                        None,
                        Some(jit_debug::JitInlineRejectionReason::StaticNative { target: kind }),
                    );
                    self.record_jit_static_native_call_plan(fid, instruction_pc, tier, kind);
                    continue;
                }
            };
            let Some(callee) = context.exec_function(callee_fid) else {
                self.record_jit_inline_candidate(
                    fid,
                    instruction_pc,
                    tier,
                    Some(callee_fid),
                    Some(jit_debug::JitInlineRejectionReason::MissingCallee),
                );
                self.record_jit_direct_call_plan(
                    jit::JitDirectCallKind::Plain,
                    fid,
                    instruction_pc,
                    tier,
                    callee_fid,
                    0,
                    1,
                    jit_debug::JitDirectCallPlanOutcome::Rejected {
                        reason: jit_debug::JitDirectCallRejectionReason::MissingCallee,
                    },
                );
                continue;
            };
            if callee.is_generator
                || callee.is_async
                || callee.is_async_generator
                || callee.needs_arguments
                || callee.has_rest
                || callee.contains_direct_eval
                || callee.is_derived_constructor
            {
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
                self.record_jit_direct_call_plan(
                    jit::JitDirectCallKind::Plain,
                    fid,
                    instruction_pc,
                    tier,
                    callee_fid,
                    0,
                    1,
                    jit_debug::JitDirectCallPlanOutcome::Rejected {
                        reason: jit_debug::JitDirectCallRejectionReason::IneligibleFunction,
                    },
                );
                continue;
            }
            let direct_call_outcome = if callee_fid == fid {
                jit_debug::JitDirectCallPlanOutcome::Rejected {
                    reason: jit_debug::JitDirectCallRejectionReason::SelfRecursive,
                }
            } else if callee.own_upvalue_count != 0 {
                jit_debug::JitDirectCallPlanOutcome::Rejected {
                    reason: jit_debug::JitDirectCallRejectionReason::OwnUpvalues {
                        count: callee.own_upvalue_count,
                    },
                }
            } else if let Some(plan) =
                self.ensure_direct_callee_plan(context, callee, eager_direct_targets)
            {
                debug_assert_eq!(plan.function_id, callee_fid);
                view.direct_callees
                    .insert(call_byte_pc, jit::JitDirectCallee { plan });
                jit_debug::JitDirectCallPlanOutcome::Available {
                    code_object_id: plan.code_object_id,
                    target_tier: match plan.tier {
                        native_abi::NativeFrameKind::Baseline => jit_debug::JitDebugTier::Template,
                        native_abi::NativeFrameKind::Optimizing => {
                            jit_debug::JitDebugTier::Optimizing
                        }
                        native_abi::NativeFrameKind::Interpreter => {
                            unreachable!("interpreter has no entry-capable code generation")
                        }
                    },
                    this_mode: plan.this_mode,
                }
            } else {
                pending_direct_targets.insert(callee_fid);
                jit_debug::JitDirectCallPlanOutcome::Rejected {
                    reason: jit_debug::JitDirectCallRejectionReason::NoEntryGeneration,
                }
            };
            self.record_jit_direct_call_plan(
                jit::JitDirectCallKind::Plain,
                fid,
                instruction_pc,
                tier,
                callee_fid,
                0,
                1,
                direct_call_outcome,
            );
            let Some(callee_view) = context.jit_compile_snapshot(callee_fid) else {
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
            view.inline_callees.insert(
                call_byte_pc,
                jit::JitInlineCallee {
                    code_block: std::sync::Arc::clone(&callee_view.code_block),
                    function_id: callee_fid,
                    param_count: callee_view.code_block.param_count,
                    register_count: callee_view.code_block.register_count,
                    instructions: callee_view.instructions,
                },
            );
        }

        // Method-call sites: snapshot monomorphic and polymorphic feedback for
        // `fid` first so the per-target `shape_offset_of` (which needs
        // `&mut self`) does not alias the feedback map borrow. Each snapshot is a
        // list of candidate targets — one for `Mono`, up to
        // `MAX_POLY_METHOD_TARGETS` (most-frequent first) for `Poly`.
        // `Megamorphic` sites are skipped and side-exit before method lookup.
        struct PolySnapshot {
            instruction_pc: u32,
            call_byte_pc: u32,
            targets: SmallVec<[PolyMethodTarget; MAX_POLY_METHOD_TARGETS]>,
        }
        let method_sites: Vec<PolySnapshot> = view
            .instructions
            .iter()
            .filter_map(|instr| {
                let site = instr.property_ic_site(&view.code_block)?;
                let state = self.method_target_feedback(site)?;
                match state {
                    MethodCallFeedback::Mono {
                        method_fid,
                        recv_shape,
                        proto_chain,
                        method_value_byte,
                    } => {
                        let mut targets: SmallVec<[PolyMethodTarget; MAX_POLY_METHOD_TARGETS]> =
                            SmallVec::new();
                        targets.push(PolyMethodTarget {
                            method_fid,
                            recv_shape,
                            proto_chain,
                            method_value_byte,
                            hits: 1,
                        });
                        Some(PolySnapshot {
                            instruction_pc: instr.instruction_pc(&view.code_block),
                            call_byte_pc: instr.byte_pc,
                            targets,
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
                        })
                    }
                    MethodCallFeedback::Megamorphic => None,
                }
            })
            .collect();
        for snap in method_sites {
            let mut direct_methods = Vec::with_capacity(snap.targets.len());
            let target_count = u32::try_from(snap.targets.len()).unwrap_or(u32::MAX);
            for (target_index, target) in snap.targets.iter().enumerate() {
                let (callee_function_id, outcome) = match self.bake_one_direct_method(
                    context,
                    fid,
                    target,
                    u32::try_from(target_index).unwrap_or(u32::MAX),
                    target_count,
                    eager_direct_targets,
                ) {
                    Ok(method) => {
                        let plan = method.callee.plan;
                        direct_methods.push(method);
                        (
                            plan.function_id,
                            jit_debug::JitDirectCallPlanOutcome::Available {
                                code_object_id: plan.code_object_id,
                                target_tier: match plan.tier {
                                    native_abi::NativeFrameKind::Baseline => {
                                        jit_debug::JitDebugTier::Template
                                    }
                                    native_abi::NativeFrameKind::Optimizing => {
                                        jit_debug::JitDebugTier::Optimizing
                                    }
                                    native_abi::NativeFrameKind::Interpreter => {
                                        unreachable!(
                                            "interpreter has no entry-capable code generation"
                                        )
                                    }
                                },
                                this_mode: jit::JitDirectCallThisMode::MethodReceiver,
                            },
                        )
                    }
                    Err(reason) => {
                        if matches!(
                            reason,
                            jit_debug::JitDirectCallRejectionReason::NoEntryGeneration
                        ) {
                            pending_direct_targets.insert(target.method_fid);
                        }
                        (
                            target.method_fid,
                            jit_debug::JitDirectCallPlanOutcome::Rejected { reason },
                        )
                    }
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
            if !direct_methods.is_empty() {
                view.direct_methods
                    .insert(snap.call_byte_pc, direct_methods);
            }
            let mut baked: Vec<jit::JitInlineMethod> = Vec::new();
            for target in &snap.targets {
                if let Some(method) = self.bake_one_inline_method(context, target) {
                    baked.push(method);
                }
            }
            match baked.len() {
                0 => {}
                // A single inlinable target remains useful even when the site
                // observed several shapes: other shapes miss its guard and
                // side-exit before method lookup.
                1 => {
                    view.inline_methods
                        .insert(snap.call_byte_pc, baked.pop().unwrap());
                }
                // Two or more: emit the guarded inline chain.
                _ => {
                    view.inline_poly_methods.insert(snap.call_byte_pc, baked);
                }
            }
        }
        if tier == jit_debug::JitDebugTier::Template
            && !self.jit_feedback_refresh_attempted.contains(&fid)
        {
            if pending_direct_targets.is_empty() {
                self.jit_pending_direct_targets.remove(&fid);
            } else {
                self.jit_pending_direct_targets
                    .insert(fid, pending_direct_targets);
            }
        }
    }

    /// Materialize one exact receiver/prototype/method-slot identity guard.
    ///
    /// Feedback keeps stable shape ids; generated code consumes compressed
    /// shape-handle offsets. Resolving every hop here keeps heap/runtime layout
    /// knowledge on the VM side.
    fn bake_method_guard(&self, target: &PolyMethodTarget) -> Option<jit::JitMethodGuard> {
        let recv_shape = self.shape_runtime.handle_for_id(target.recv_shape)?;
        let proto_chain = target
            .proto_chain
            .as_slice()
            .iter()
            .map(|shape_id| {
                self.shape_runtime
                    .handle_for_id(*shape_id)
                    .map(|shape| shape.offset())
            })
            .collect::<Option<Vec<_>>>()?;
        Some(jit::JitMethodGuard {
            method_fid: target.method_fid,
            recv_shape: recv_shape.offset(),
            proto_chain,
            method_value_byte: target.method_value_byte,
        })
    }

    /// Bake one compiler-generated method call independently of leaf inlining.
    ///
    /// Only a monomorphic feedback target reaches this helper. The target must
    /// be an ordinary synchronous function, differ from the caller, require no
    /// fresh capture-cell allocation, and already have one entry-capable native
    /// generation. Inherited closure captures are consumed directly.
    fn bake_one_direct_method(
        &mut self,
        context: &ExecutionContext,
        caller_fid: u32,
        target: &PolyMethodTarget,
        target_index: u32,
        target_count: u32,
        eager_direct_targets: bool,
    ) -> Result<jit::JitDirectMethod, jit_debug::JitDirectCallRejectionReason> {
        let method = context
            .exec_function(target.method_fid)
            .ok_or(jit_debug::JitDirectCallRejectionReason::MissingCallee)?;
        if method.is_generator
            || method.is_async
            || method.is_async_generator
            || method.needs_arguments
            || method.has_rest
            || method.contains_direct_eval
            || method.is_derived_constructor
        {
            return Err(jit_debug::JitDirectCallRejectionReason::IneligibleFunction);
        }
        if target.method_fid == caller_fid {
            return Err(jit_debug::JitDirectCallRejectionReason::SelfRecursive);
        }
        if method.own_upvalue_count != 0 {
            return Err(jit_debug::JitDirectCallRejectionReason::OwnUpvalues {
                count: method.own_upvalue_count,
            });
        }
        let guard = self
            .bake_method_guard(target)
            .ok_or(jit_debug::JitDirectCallRejectionReason::MethodGuardUnavailable)?;
        let plan = self
            .ensure_direct_callee_plan(context, method, eager_direct_targets)
            .ok_or(jit_debug::JitDirectCallRejectionReason::NoEntryGeneration)?;
        debug_assert_eq!(plan.function_id, target.method_fid);
        Ok(jit::JitDirectMethod {
            target_index,
            target_count,
            guard,
            callee: jit::JitDirectCallee { plan },
        })
    }

    /// Bake one inline-method candidate body for a `(method, receiver shape)`
    /// target, resolving its sealed property loads/stores to value-slab byte
    /// offsets against the receiver shape. Returns `None` when the method shape
    /// is ineligible (generator/async/derived-constructor/etc.), its view is
    /// missing, or any body property fails to resolve to a sealed receiver slot.
    /// Shared by the monomorphic and polymorphic method-inline bake paths.
    pub(crate) fn bake_one_inline_method(
        &mut self,
        context: &ExecutionContext,
        target: &PolyMethodTarget,
    ) -> Option<jit::JitInlineMethod> {
        self.bake_inline_method_rec(context, target, 0)
    }

    /// Recursion bound for nested method-body inlining. A method whose tail is a
    /// call splices that callee's body; the callee may in turn call, so the bake
    /// recurses down the monomorphic call chain to this depth (richards is
    /// `run → task.run → scheduler.X`). Deeper calls side-exit if reached.
    const MAX_INLINE_METHOD_DEPTH: u32 = 3;

    pub(crate) fn bake_inline_method_rec(
        &mut self,
        context: &ExecutionContext,
        target: &PolyMethodTarget,
        depth: u32,
    ) -> Option<jit::JitInlineMethod> {
        let method = context.exec_function(target.method_fid)?;
        if method.is_generator
            || method.is_async
            || method.is_async_generator
            || method.needs_arguments
            || method.has_rest
            || method.contains_direct_eval
            || method.is_derived_constructor
            || method.makes_function
        {
            return None;
        }
        let method_view = context.jit_compile_snapshot(target.method_fid)?;
        // Resolve every body `LoadProperty`/`StoreProperty` to a sealed value
        // byte offset; bail out if any property is absent, an accessor, or spills
        // past the inline value capacity. A receiver property resolves against
        // the identity-guarded receiver shape (no per-op guard); a non-receiver
        // property falls back to its own monomorphic site feedback and records
        // the shape the inliner must guard. Loads carry the name at operand 2,
        // stores at operand 1.
        let mut prop_offsets: rustc_hash::FxHashMap<u32, u32> = rustc_hash::FxHashMap::default();
        let mut prop_shapes: rustc_hash::FxHashMap<u32, u32> = rustc_hash::FxHashMap::default();
        const SLOT_BYTES: u32 =
            std::mem::size_of::<crate::value::compressed::CompressedValue>() as u32;
        for instr in &method_view.instructions {
            let name_operand = match instr.op(&method_view.code_block) {
                Op::LoadProperty => 2,
                Op::StoreProperty => 1,
                _ => continue,
            };
            let otter_bytecode::Operand::ConstIndex(name_idx) =
                instr.operand(&method_view.code_block, name_operand)?
            else {
                return None;
            };
            let key = context.property_atom(name_idx)?;
            let recv_shape = self.shape_runtime.handle_for_id(target.recv_shape)?;
            if let Some(slot) = self.shape_offset_of(recv_shape, key.name()) {
                prop_offsets.insert(instr.byte_pc, slot * SLOT_BYTES);
                continue;
            }
            // Not a receiver property: use the op's own monomorphic own-data site
            // feedback (shape offset, slot byte). Anything else — polymorphic,
            // prototype, accessor, or unobserved — is not inlinable.
            let site = instr.property_ic_site(&method_view.code_block)?;
            let (shape_off, value_byte) =
                self.monomorphic_own_property_feedback(instr.op(&method_view.code_block), site)?;
            prop_offsets.insert(instr.byte_pc, value_byte);
            prop_shapes.insert(instr.byte_pc, shape_off);
        }
        // Recursively bake the body's monomorphic nested method calls so the
        // inliner can splice their bodies. Only `Mono` sites recurse;
        // polymorphic/megamorphic internal calls stay uninlined.
        // Collect targets first — the recursion needs `&mut self`, which cannot
        // overlap the feedback-map borrow.
        let mut nested_targets: Vec<(u32, PolyMethodTarget)> = Vec::new();
        if depth < Self::MAX_INLINE_METHOD_DEPTH {
            for instr in &method_view.instructions {
                if instr.op(&method_view.code_block) != Op::CallMethodValue {
                    continue;
                }
                let Some(site) = instr.property_ic_site(&method_view.code_block) else {
                    continue;
                };
                if let Some(MethodCallFeedback::Mono {
                    method_fid,
                    recv_shape,
                    proto_chain,
                    method_value_byte,
                }) = self.method_target_feedback(site)
                {
                    nested_targets.push((
                        instr.byte_pc,
                        PolyMethodTarget {
                            method_fid,
                            recv_shape,
                            proto_chain,
                            method_value_byte,
                            hits: 1,
                        },
                    ));
                }
            }
        }
        let mut nested_methods: rustc_hash::FxHashMap<u32, jit::JitInlineMethod> =
            rustc_hash::FxHashMap::default();
        for (pc, nested_target) in nested_targets {
            if let Some(nested) = self.bake_inline_method_rec(context, &nested_target, depth + 1) {
                nested_methods.insert(pc, nested);
            }
        }
        let guard = self.bake_method_guard(target)?;
        Some(jit::JitInlineMethod {
            code_block: std::sync::Arc::clone(&method_view.code_block),
            guard,
            param_count: method_view.code_block.param_count,
            register_count: method_view.code_block.register_count,
            instructions: method_view.instructions,
            prop_offsets,
            prop_shapes,
            nested_methods,
        })
    }

    /// Bake JIT-readable collection leaf method IC metadata.
    ///
    /// The emitted baseline guard still validates receiver type, no
    /// prototype/expando override, prototype shape, and builtin identity at the
    /// slot. Baking only makes those fields machine-readable so the hot path no
    /// longer crosses into Rust just to resolve a `RuntimeStubId`.
    /// Bake dense-array `push` / `pop` method-call guard metadata so the
    /// baseline can splice an inline fast path for the site. Guard misses
    /// side-exit before method effects.
    ///
    /// `pop` runs as a mutating leaf and needs no root map. `push` may grow the
    /// dense buffer, so its site reserves a frame-slot safepoint the same way
    /// the allocating collection writes do.
    pub(crate) fn bake_array_methods(&self, view: &mut jit::JitCompileSnapshot) {
        for instr in &view.instructions {
            if instr.op(&view.code_block) != Op::CallMethodValue {
                continue;
            }
            let Some(site) = instr.property_ic_site(&view.code_block) else {
                continue;
            };
            let push_safepoint_id = view.safepoints.len() as native_abi::SafepointId;
            let Some(feedback) = self.jit_array_method_feedback(site, push_safepoint_id) else {
                continue;
            };
            if feedback.kind.allocates() {
                if !crate::runtime_stubs::alloc_value_stub_by_id(feedback.stub_id)
                    .is_some_and(|stub| stub.is_valid_for_safepoint(push_safepoint_id))
                {
                    continue;
                }
                view.safepoints.insert(
                    push_safepoint_id,
                    native_abi::SafepointRecord::frame_slot_window(
                        push_safepoint_id,
                        native_abi::NO_FRAME_STATE,
                        view.code_block.register_count,
                    ),
                );
            } else if !crate::runtime_stubs::mutating_leaf_stub2_by_id(feedback.stub_id)
                .is_some_and(crate::runtime_stubs::MutatingLeafStub2::is_valid)
            {
                continue;
            }
            view.array_methods.insert(instr.byte_pc, feedback);
        }
    }

    pub(crate) fn bake_primitive_method_guards(&self, view: &mut jit::JitCompileSnapshot) {
        for instr in &view.instructions {
            if instr.op(&view.code_block) != Op::CallMethodValue {
                continue;
            }
            let Some(feedback) = self.jit_primitive_method_guard(instr.method_hint) else {
                continue;
            };
            view.primitive_method_guards.insert(instr.byte_pc, feedback);
        }
    }

    pub(crate) fn bake_collection_leaf_methods(&self, view: &mut jit::JitCompileSnapshot) {
        for instr in &view.instructions {
            if instr.op(&view.code_block) != Op::CallMethodValue {
                continue;
            }
            let Some(site) = instr.property_ic_site(&view.code_block) else {
                continue;
            };
            let Some(feedback) = self.jit_collection_leaf_method_feedback(site) else {
                continue;
            };
            view.collection_leaf_methods.insert(instr.byte_pc, feedback);
        }
    }

    /// Bake JIT-readable collection allocating method IC metadata.
    ///
    /// This only publishes guard metadata and the target `AllocStub` descriptor
    /// id. Baseline codegen must continue using the rooted fallback until it can
    /// attach exact safepoint maps to the machine call site.
    pub(crate) fn bake_collection_alloc_methods(&self, view: &mut jit::JitCompileSnapshot) {
        for instr in &view.instructions {
            if instr.op(&view.code_block) != Op::CallMethodValue {
                continue;
            }
            let Some(site) = instr.property_ic_site(&view.code_block) else {
                continue;
            };
            let safepoint_id = view.safepoints.len() as native_abi::SafepointId;
            let Some(feedback) = self.jit_collection_alloc_method_feedback(site, safepoint_id)
            else {
                continue;
            };
            if !crate::runtime_stubs::alloc_value_stub_by_id(feedback.alloc_stub_id)
                .is_some_and(|stub| stub.is_valid_for_safepoint(safepoint_id))
            {
                continue;
            }
            view.safepoints.insert(
                safepoint_id,
                native_abi::SafepointRecord::frame_slot_window(
                    safepoint_id,
                    native_abi::NO_FRAME_STATE,
                    view.code_block.register_count,
                ),
            );
            view.collection_alloc_methods
                .insert(instr.byte_pc, feedback);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::Interpreter;

    #[test]
    fn fresh_interpreter_has_no_executable_code_residency() {
        let interpreter = Interpreter::new();
        assert_eq!(interpreter.jit_code_residency().code_bytes, 0);
        assert_eq!(interpreter.jit_code_residency().unique_code_objects, 0);
    }
}
