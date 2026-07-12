//! JIT compile requests and cold profile-feedback baking.
//!
//! # Contents
//! - `jit_code_residency` — opt-in whole-isolate executable-code snapshot.
//! - `compile_jit_function` and cold feedback baking into the instruction view
//!   (property/object-literal/inline-callee tables).
//! - Call/method target profiling and reoptimization eviction.
//!
//! # Invariants
//! Baked pointers (shape ids, global cells, prototype slots) must only
//! reference permanent or non-moving allocations; anything movable goes
//! through a runtime stub instead.
#![allow(unused_imports)]
use crate::*;

impl Interpreter {
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
        for code in self.jit_osr_code.values().flatten() {
            record(code);
        }
        if let Some((_, code)) = &self.jit_code_cache {
            record(code);
        }
        for (_, code) in &self.jit_direct_code_anchors {
            record(code);
        }
        for cache_set in &self.jit_direct_method_cache {
            for entry in cache_set {
                record(&entry.code);
            }
        }

        jit::JitCodeResidency {
            installed_entry_bodies: self.jit_code.values().flatten().count() as u64,
            installed_osr_bodies: self.jit_osr_code.values().flatten().count() as u64,
            unique_code_objects: seen.len() as u64,
            code_bytes,
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
        let mut view = context.jit_compile_snapshot(fid)?;
        Self::bake_typed_array_layout(&mut view);
        Self::bake_string_layout(&mut view);
        self.bake_inline_callees(&mut view, context, fid);
        self.bake_collection_leaf_methods(&mut view);
        self.bake_collection_alloc_methods(&mut view);
        self.bake_array_methods(&mut view);
        self.bake_primitive_method_guards(&mut view);
        let trace = std::env::var_os("OTTER_JIT_TRACE").is_some();
        if trace {
            let function_name = context
                .function(fid)
                .map(|function| function.name.as_str())
                .unwrap_or("<unknown>");
            let method_feedback = view
                .instructions
                .iter()
                .filter(|instr| {
                    instr
                        .property_ic_site(&view.code_block)
                        .is_some_and(|site| {
                            self.jit_method_site_feedback
                                .get(site)
                                .is_some_and(Option::is_some)
                        })
                })
                .count();
            let call_feedback = view
                .instructions
                .iter()
                .filter(|instr| {
                    view.code_block
                        .feedback_at(instr.instruction_pc(&view.code_block) as usize)
                        .is_some_and(|cell| cell.call_target().is_some())
                })
                .count();
            eprintln!(
                "[otter-jit] view fid {fid} {function_name}: call_feedback={} method_feedback={} inline_callees={} inline_methods={}",
                call_feedback,
                method_feedback,
                view.inline_callees.len(),
                view.inline_methods.len()
            );
        }
        let (regs, params) = (view.code_block.register_count, view.code_block.param_count);
        let hook = self.jit_hook.as_ref()?.clone();
        let status = hook.compile_function(jit::JitCompileRequest {
            snapshot: view,
            osr_pc,
        });
        if trace {
            eprintln!("[jit] compile fid={fid} regs={regs} params={params} -> {status:?}");
        }
        match status {
            Ok(jit::JitCompileStatus::Compiled { code }) => Some(code),
            _ => None,
        }
    }

    /// Bake the static heap-layout offsets for the JIT's inline typed-array
    /// element fast path, and ensure `cage_base` is set so the emitter enables
    /// inline `LoadElement`/`StoreElement` (the offsets are isolate-independent
    /// `#[repr(C)]` constants, but inline access still needs the cage base to
    /// decompress receiver / buffer pointers).
    pub(crate) fn bake_typed_array_layout(view: &mut jit::JitCompileSnapshot) {
        use crate::binary::array_buffer as ab;
        use crate::binary::typed_array as ta;
        let header = otter_gc::header::HEADER_SIZE as u32;
        let buffer_base = header + ta::TYPED_ARRAY_BODY_BUFFER_OFFSET as u32;
        view.ta_layout = jit::JitTypedArrayLayout {
            ta_type_tag: ta::TYPED_ARRAY_BODY_TYPE_TAG,
            local_buffer_type_tag: ab::LOCAL_ARRAY_BUFFER_BODY_TYPE_TAG,
            kind_float64: ta::TypedArrayKind::Float64 as u32,
            kind_int32: ta::TypedArrayKind::Int32 as u32,
            buffer_local_tag: ab::BUFFER_STORAGE_LOCAL_TAG,
            ta_kind_byte: header + ta::TYPED_ARRAY_BODY_KIND_OFFSET as u32,
            ta_byte_offset_byte: header + ta::TYPED_ARRAY_BODY_BYTE_OFFSET_OFFSET as u32,
            ta_length_byte: header + ta::TYPED_ARRAY_BODY_LENGTH_OFFSET as u32,
            ta_length_tracking_byte: header + ta::TYPED_ARRAY_BODY_LENGTH_TRACKING_OFFSET as u32,
            buffer_disc_byte: buffer_base + ab::BUFFER_STORAGE_DISCRIMINANT_OFFSET as u32,
            buffer_handle_byte: buffer_base + ab::BUFFER_STORAGE_HANDLE_OFFSET as u32,
            // Vec<u8> base; otter-jit adds the probed ptr/len word sub-offsets.
            buf_bytes_byte: header + ab::LOCAL_ARRAY_BUFFER_BODY_BYTES_OFFSET as u32,
            array_type_tag: crate::array::ARRAY_BODY_TYPE_TAG,
            array_elements_byte: header + crate::array::ARRAY_BODY_ELEMENTS_OFFSET as u32,
            array_length_byte: header + crate::array::ARRAY_BODY_LENGTH_OFFSET as u32,
            array_exotic_byte: header
                + std::mem::offset_of!(crate::array::ArrayBody, exotic) as u32,
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
        let entry = match op {
            Op::LoadProperty => self.load_property_ics.get(site)?,
            Op::StoreProperty => self.store_property_ics.get(site)?,
            _ => return None,
        };
        let [stub] = entry.entries() else {
            return None;
        };
        let hit = match op {
            Op::LoadProperty => stub.own_data_hit()?,
            Op::StoreProperty => stub.store_own_data_hit()?,
            _ => unreachable!(),
        };
        (!hit.shape.is_null()).then_some((hit.shape.offset(), u32::from(hit.slot) * SLOT_BYTES))
    }

    /// Whether a method-call site's feedback has already saturated to
    /// `Megamorphic`. Once it has, further [`Self::note_method_target`]
    /// observations are no-ops, so a caller can skip the receiver/prototype
    /// shape walk that only exists to build the `MethodSite` argument — the hot
    /// path for a megamorphic site (e.g. one `arr[i].run()` over many classes).
    pub(crate) fn method_site_feedback_saturated(&self, site: usize) -> bool {
        matches!(
            self.jit_method_site_feedback.get(site),
            Some(Some(MethodCallFeedback::Megamorphic))
        )
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
        let new_target = PolyMethodTarget {
            method_fid,
            recv_shape: site.recv_shape,
            proto_chain: site.proto_chain,
            method_value_byte: site.method_value_byte,
            hits: 1,
        };
        let Some(slot) = self.jit_method_site_feedback.get_mut(feedback_site) else {
            return;
        };
        match slot {
            None => {
                *slot = Some(MethodCallFeedback::Mono {
                    method_fid,
                    recv_shape: site.recv_shape,
                    proto_chain: site.proto_chain,
                    method_value_byte: site.method_value_byte,
                });
            }
            Some(state) => match state {
                MethodCallFeedback::Mono {
                    method_fid: seen_fid,
                    recv_shape: seen_shape,
                    proto_chain: seen_proto_chain,
                    method_value_byte: seen_value_byte,
                } => {
                    let same = *seen_fid == method_fid
                        && seen_shape.offset() == site.recv_shape.offset()
                        && seen_proto_chain.same(&site.proto_chain)
                        && *seen_value_byte == site.method_value_byte;
                    if !same {
                        let prior = PolyMethodTarget {
                            method_fid: *seen_fid,
                            recv_shape: *seen_shape,
                            proto_chain: *seen_proto_chain,
                            method_value_byte: *seen_value_byte,
                            hits: 1,
                        };
                        let mut targets: SmallVec<[PolyMethodTarget; MAX_POLY_METHOD_TARGETS]> =
                            SmallVec::new();
                        targets.push(prior);
                        targets.push(new_target);
                        *state = MethodCallFeedback::Poly(Box::new(targets));
                    }
                }
                MethodCallFeedback::Poly(targets) => {
                    if let Some(existing) =
                        targets.iter_mut().find(|t| t.matches(method_fid, &site))
                    {
                        existing.hits = existing.hits.saturating_add(1);
                    } else if targets.len() < MAX_POLY_METHOD_TARGETS {
                        targets.push(new_target);
                    } else {
                        *state = MethodCallFeedback::Megamorphic;
                    }
                }
                MethodCallFeedback::Megamorphic => {}
            },
        }
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
        let recv_shape = crate::object::shape(recv, &self.gc_heap);
        if recv_shape.is_null() {
            return None;
        }
        let slot_byte = |slot: u32| {
            slot * std::mem::size_of::<crate::value::compressed::CompressedValue>() as u32
        };
        if let Some(slot) = self.shape_offset_of(recv_shape, name.name()) {
            return Some(MethodSite {
                recv_shape,
                proto_chain: crate::MethodProtoChain::own(),
                method_value_byte: slot_byte(slot),
            });
        }
        // Walk the prototype chain, recording each hopped object's shape; the
        // baked guard replays exactly this walk (flat-prototype chase + shape
        // compare per hop) before trusting the holder's slot offset.
        let mut proto_chain = crate::MethodProtoChain::own();
        let mut cur = recv;
        loop {
            cur = crate::object::prototype(cur, &self.gc_heap)?;
            let shape = crate::object::shape(cur, &self.gc_heap);
            if shape.is_null() || !proto_chain.push(shape) {
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
        self.jit_code.remove(&fid);
        self.jit_entry_osr_only.remove(&fid);
        self.jit_code_cache = None;
        self.clear_jit_direct_method_cache_for_fid(fid);
        self.jit_osr_code.retain(|&(f, _), _| f != fid);
        self.jit_osr_disabled.retain(|&(f, _)| f != fid);
        self.jit_osr_counts.retain(|&(f, _), _| f != fid);
    }

    /// Bake inline-candidate callee bodies for `fid`'s monomorphic `Op::Call`
    /// sites into `view`, so the baseline can splice a tiny leaf callee under an
    /// identity guard instead of emitting the per-call bridge.
    ///
    /// A site is a candidate only when (a) it observed a single callee (`Mono`),
    /// and (b) that callee is a plain synchronous bytecode function — the same
    /// shape the direct-call bridge accepts. The emitter applies the final
    /// pure-leaf / size / arity test; `Poly`, unobserved, and disqualified-shape
    /// sites are left out and emit the normal bridge.
    pub(crate) fn bake_inline_callees(
        &mut self,
        view: &mut jit::JitCompileSnapshot,
        context: &ExecutionContext,
        fid: u32,
    ) {
        let trace = std::env::var_os("OTTER_JIT_TRACE").is_some();
        let call_sites: Vec<_> = view
            .instructions
            .iter()
            .filter_map(|instr| {
                let instruction_pc = instr.instruction_pc(&view.code_block);
                let state = view
                    .code_block
                    .feedback_at(instruction_pc as usize)?
                    .call_target()?;
                Some((instruction_pc, instr.byte_pc, state))
            })
            .collect();
        for (instruction_pc, call_byte_pc, state) in call_sites {
            let CallTargetFeedback::Mono(callee_fid) = state else {
                if trace {
                    eprintln!("[otter-jit] inline callee skip fid {fid} pc {instruction_pc}: poly");
                }
                continue;
            };
            let Some(callee) = context.exec_function(callee_fid) else {
                if trace {
                    eprintln!(
                        "[otter-jit] inline callee skip fid {fid} pc {instruction_pc}: missing callee {callee_fid}"
                    );
                }
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
                if trace {
                    eprintln!(
                        "[otter-jit] inline callee skip fid {fid} pc {instruction_pc}: ineligible callee {callee_fid} flags gen={} async={} async_gen={} args={} rest={} eval={} derived={} makes_fn={}",
                        callee.is_generator,
                        callee.is_async,
                        callee.is_async_generator,
                        callee.needs_arguments,
                        callee.has_rest,
                        callee.contains_direct_eval,
                        callee.is_derived_constructor,
                        callee.makes_function,
                    );
                }
                continue;
            }
            let Some(callee_view) = context.jit_compile_snapshot(callee_fid) else {
                if trace {
                    eprintln!(
                        "[otter-jit] inline callee skip fid {fid} pc {instruction_pc}: missing view {callee_fid}"
                    );
                }
                continue;
            };
            if trace {
                eprintln!(
                    "[otter-jit] inline callee bake fid {fid} pc {instruction_pc}: callee {callee_fid}"
                );
            }
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
        // `Megamorphic` sites are skipped and take the in-place method bridge.
        struct PolySnapshot {
            instruction_pc: u32,
            targets: SmallVec<[PolyMethodTarget; MAX_POLY_METHOD_TARGETS]>,
        }
        let method_feedback = &self.jit_method_site_feedback;
        let method_sites: Vec<PolySnapshot> = view
            .instructions
            .iter()
            .filter_map(|instr| {
                let instruction_pc = instr.instruction_pc(&view.code_block);
                let site = instr.property_ic_site(&view.code_block)?;
                let state = method_feedback.get(site)?.as_ref()?;
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
                            method_fid: *method_fid,
                            recv_shape: *recv_shape,
                            proto_chain: *proto_chain,
                            method_value_byte: *method_value_byte,
                            hits: 1,
                        });
                        Some(PolySnapshot {
                            instruction_pc,
                            targets,
                        })
                    }
                    MethodCallFeedback::Poly(observed) => {
                        let mut targets = (**observed).clone();
                        // Most-frequent target first: the common receiver shape
                        // then hits the shortest guard chain.
                        targets.sort_by_key(|t| std::cmp::Reverse(t.hits));
                        Some(PolySnapshot {
                            instruction_pc,
                            targets,
                        })
                    }
                    MethodCallFeedback::Megamorphic => None,
                }
            })
            .collect();
        for snap in method_sites {
            let Some(call_byte_pc) = view
                .instructions
                .iter()
                .find(|instr| instr.instruction_pc(&view.code_block) == snap.instruction_pc)
                .map(|instr| instr.byte_pc)
            else {
                continue;
            };
            let mut baked: Vec<jit::JitInlineMethod> = Vec::new();
            for target in &snap.targets {
                if let Some(method) = self.bake_one_inline_method(context, target) {
                    baked.push(method);
                }
            }
            match baked.len() {
                0 => {}
                // A single inlinable target is the monomorphic fast path, even if
                // the site observed several shapes: the others miss its guard and
                // take the bridge, which is strictly better than no inline.
                1 => {
                    view.inline_methods
                        .insert(call_byte_pc, baked.pop().unwrap());
                }
                // Two or more: emit the guarded inline chain.
                _ => {
                    view.inline_poly_methods.insert(call_byte_pc, baked);
                }
            }
        }
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
    /// `run → task.run → scheduler.X`), then leaves deeper calls bridged.
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
            if let Some(slot) = self.shape_offset_of(target.recv_shape, key.name()) {
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
        // inliner can splice their bodies rather than bridge them. Only `Mono`
        // sites recurse; polymorphic/megamorphic internal calls stay bridged.
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
                }) = self
                    .jit_method_site_feedback
                    .get(site)
                    .and_then(Option::as_ref)
                {
                    nested_targets.push((
                        instr.byte_pc,
                        PolyMethodTarget {
                            method_fid: *method_fid,
                            recv_shape: *recv_shape,
                            proto_chain: *proto_chain,
                            method_value_byte: *method_value_byte,
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
        Some(jit::JitInlineMethod {
            code_block: std::sync::Arc::clone(&method_view.code_block),
            method_fid: target.method_fid,
            recv_shape: target.recv_shape.offset(),
            proto_chain: target
                .proto_chain
                .as_slice()
                .iter()
                .map(|s| s.offset())
                .collect(),
            method_value_byte: target.method_value_byte,
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
    /// baseline can splice an inline fast path for the site. The runtime method
    /// bridge re-validates the receiver/prototype/builtin on every miss.
    pub(crate) fn bake_array_methods(&self, view: &mut jit::JitCompileSnapshot) {
        for instr in &view.instructions {
            if instr.op(&view.code_block) != Op::CallMethodValue {
                continue;
            }
            let Some(site) = instr.property_ic_site(&view.code_block) else {
                continue;
            };
            if let Some(feedback) = self.jit_array_method_feedback(site) {
                view.array_methods.insert(instr.byte_pc, feedback);
            }
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
