//! Optimizing-tier compile requests and profile-feedback baking.
//!
//! # Contents
//! - `jit_code_residency` — opt-in whole-isolate executable-code snapshot.
//! - `compile_jit_function` and feedback baking into the instruction view
//!   (arith/element/property/global-cell/object-literal/inline-callee tables).
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
        self.bake_arith_feedback(&mut view, fid);
        self.bake_element_load_kind(&mut view, fid);
        self.bake_property_feedback(&mut view);
        self.bake_global_lex_cells(&mut view, context);
        self.bake_object_literals(&mut view, context);
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
            let method_feedback = self
                .jit_method_site_feedback
                .iter()
                .filter(|&(&(caller_fid, _), _)| caller_fid == fid)
                .count();
            eprintln!(
                "[otter-jit] view fid {fid} {function_name}: call_feedback={} method_feedback={} inline_callees={} inline_methods={}",
                self.jit_call_site_feedback
                    .iter()
                    .filter(|&(&(caller_fid, _), _)| caller_fid == fid)
                    .count(),
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

    /// Fold the operand representations of one observed arithmetic / relational
    /// execution into the optimizing-tier type-feedback cell for the currently
    /// dispatching site (`current_function_id`, `current_instruction_pc`). Called from
    /// the arithmetic opcode helpers; gated by the caller on a JIT hook being
    /// installed, so interpreter-only execution records nothing. The cell is
    /// baked into the compile snapshot at tier-up.
    #[inline]
    pub(crate) fn note_arith(&mut self, lhs: Value, rhs: Value) {
        self.jit_arith_feedback
            .entry((self.current_function_id, self.current_instruction_pc))
            .or_default()
            .record(lhs, rhs);
    }

    /// Record the receiver kind observed at the current `LoadElement` site so a
    /// site that reads only from a single unboxable typed-array kind
    /// (`Float64Array` / `Int32Array`) can lower to a native-representation load.
    /// Any other receiver demotes the site to
    /// [`jit::JitElementLoadKind::Any`] (generic boxed load), and that demotion
    /// is sticky — a mixed site never re-specializes.
    pub(crate) fn note_element_load(&mut self, recv: Value) {
        let key = (self.current_function_id, self.current_instruction_pc);
        let observed = match recv.as_typed_array(&self.gc_heap).map(|t| t.kind()) {
            Some(crate::binary::TypedArrayKind::Float64) => jit::JitElementLoadKind::Float64,
            Some(crate::binary::TypedArrayKind::Int32) => jit::JitElementLoadKind::Int32,
            Some(_) => jit::JitElementLoadKind::Any,
            None => {
                // A non-typed-array receiver only matters if the site had already
                // specialized: demote it so the tier keeps the generic load.
                if let Some(slot) = self.jit_element_load_kind.get_mut(&key) {
                    *slot = jit::JitElementLoadKind::Any;
                }
                return;
            }
        };
        self.jit_element_load_kind
            .entry(key)
            .and_modify(|slot| {
                if *slot != observed {
                    *slot = jit::JitElementLoadKind::Any;
                }
            })
            .or_insert(observed);
    }

    /// Copy the warmup element-load kind feedback recorded for `fid`'s
    /// `LoadElement` sites into the compile snapshot, keyed by instruction PC. Sites the
    /// interpreter never observed (or observed with mixed receivers) stay
    /// [`jit::JitElementLoadKind::Any`], which lowers as the generic boxed load.
    pub(crate) fn bake_element_load_kind(&self, view: &mut jit::JitCompileSnapshot, fid: u32) {
        if self.jit_element_load_kind.is_empty() {
            return;
        }
        for instr in &mut view.instructions {
            if instr.op != Op::LoadElement {
                continue;
            }
            if let Some(kind) = self.jit_element_load_kind.get(&(fid, instr.instruction_pc)) {
                instr.element_load_kind = *kind;
            }
        }
    }

    /// Copy the warmup value-representation feedback recorded for `fid`'s
    /// numeric-specialized sites into the compile snapshot, keyed by each
    /// instruction's canonical PC. Sites the interpreter never observed stay `0`
    /// (unknown), which the optimizing tier lowers generically.
    pub(crate) fn bake_arith_feedback(&self, view: &mut jit::JitCompileSnapshot, fid: u32) {
        if self.jit_arith_feedback.is_empty() && self.jit_arith_widen_float.is_empty() {
            return;
        }
        for instr in &mut view.instructions {
            if self
                .jit_arith_widen_float
                .contains(&(fid, instr.instruction_pc))
            {
                instr.arith_feedback = jit_feedback::ARITH_INT32 | jit_feedback::ARITH_FLOAT64;
            } else if let Some(fb) = self.jit_arith_feedback.get(&(fid, instr.instruction_pc)) {
                instr.arith_feedback = fb.bits();
            }
        }
    }

    /// Copy JIT-readable property IC cases into the compile snapshot. Own-data
    /// loads/stores bake as receiver-shape slot accesses; a single direct
    /// prototype data load bakes as receiver/prototype shape guards plus the
    /// prototype slot. Megamorphic, accessor, dictionary, mixed, and deeper
    /// prototype sites stay empty and lower through the property bridge.
    pub(crate) fn bake_property_feedback(&self, view: &mut jit::JitCompileSnapshot) {
        // Byte size of one compressed slot — a `hit.slot` index scales by this to
        // the value's byte offset inside the object's value slab.
        const SLOT_BYTES: u32 =
            std::mem::size_of::<crate::value::compressed::CompressedValue>() as u32;
        for instr in &mut view.instructions {
            let Some(site) = instr.property_ic_site else {
                continue;
            };
            // Collect every entry's own-data `(shape_offset, slot_byte)` case, or
            // `None` if any entry is not an own-data hit (a prototype / accessor /
            // transition stub the guard chain cannot represent) — in which case the
            // whole site is left uncached for the tier. A dictionary-mode hit
            // carries a null shape handle; its compressed offset is 0, which is
            // also what every dictionary-mode object stores in its shape field, so
            // baking it would produce a guard that matches *any* dictionary object
            // and an inline slot access against a layout that is not
            // shape-stable. Such a hit disqualifies the whole site.
            let jit_hit = |hit: crate::object::AtomOwnPropertyHit| {
                (!hit.shape.is_null())
                    .then_some((hit.shape.offset(), u32::from(hit.slot) * SLOT_BYTES))
            };
            let cases: Option<Vec<(u32, u32)>> = match instr.op {
                otter_bytecode::Op::LoadProperty => self.load_property_ics.get(site).map(|e| {
                    e.entries()
                        .iter()
                        .map(|stub| stub.own_data_hit().and_then(jit_hit))
                        .collect::<Option<Vec<_>>>()
                }),
                otter_bytecode::Op::StoreProperty => self.store_property_ics.get(site).map(|e| {
                    e.entries()
                        .iter()
                        .map(|stub| stub.store_own_data_hit().and_then(jit_hit))
                        .collect::<Option<Vec<_>>>()
                }),
                _ => None,
            }
            .flatten();

            // One own-data shape → the monomorphic guard the tier already lowers.
            // 2..=cap own-data shapes → the polymorphic guard chain. Beyond the cap
            // (or a non-own-data mix) both stay empty and the site keeps the
            // interpreter IC.
            match cases.as_deref() {
                Some([one]) => {
                    instr.property_feedback = Some(*one);
                    instr.property_feedback_poly = Vec::new();
                    instr.property_proto_feedback = None;
                }
                Some(many) if (2..=MAX_POLY_PROPERTY_CASES).contains(&many.len()) => {
                    instr.property_feedback = None;
                    instr.property_feedback_poly = many.to_vec();
                    instr.property_proto_feedback = None;
                }
                _ => {
                    instr.property_feedback = None;
                    instr.property_feedback_poly = Vec::new();
                    instr.property_proto_feedback = None;
                }
            }

            if !matches!(instr.op, otter_bytecode::Op::LoadProperty)
                || instr.property_feedback.is_some()
                || !instr.property_feedback_poly.is_empty()
            {
                continue;
            }

            let proto_cases = self.load_property_ics.get(site).map(|e| {
                e.entries()
                    .iter()
                    .map(|stub| {
                        stub.direct_prototype_load_jit()
                            .and_then(|(recv_shape, hit)| {
                                // Same null-shape rule as the own-data cases above:
                                // a dictionary-mode receiver or holder cannot be
                                // shape-guarded, so it disqualifies the site.
                                (!recv_shape.is_null() && !hit.shape.is_null()).then_some((
                                    recv_shape.offset(),
                                    hit.shape.offset(),
                                    u32::from(hit.slot) * SLOT_BYTES,
                                ))
                            })
                    })
                    .collect::<Option<Vec<_>>>()
            });
            if let Some(Some(proto_cases)) = proto_cases
                && let [one] = proto_cases.as_slice()
            {
                instr.property_proto_feedback = Some(*one);
            }
        }
    }

    /// Resolve each `LoadGlobalOrThrow` site whose free identifier is a global
    /// declarative-record (lexical) binding to that binding's cell, baking the
    /// cell's compressed offset onto the instruction. The optimizing tier then
    /// reads the value inline (`cage_base + offset`, one load, TDZ-hole guard)
    /// instead of the per-access global-load bridge. Names that are not lexical
    /// bindings (a `var` or a plain global-object property) or that are unbound
    /// at compile time are left `None`, keeping the bridge for those sites.
    pub(crate) fn bake_global_lex_cells(
        &self,
        view: &mut jit::JitCompileSnapshot,
        context: &ExecutionContext,
    ) {
        use otter_bytecode::{Op, Operand};
        let fid = view.code_block.id;
        let code_block = std::sync::Arc::clone(&view.code_block);
        for instr in &mut view.instructions {
            if instr.op != Op::LoadGlobalOrThrow {
                continue;
            }
            let Some(Operand::ConstIndex(name_idx)) = code_block.operand(instr, 1) else {
                continue;
            };
            let Some(name) = context.string_constant_str_for_function(fid, name_idx) else {
                continue;
            };
            // A lexical hit shadows the global object (§9.1.1.4), so only these
            // resolve to a stable cell. The value read from the cell is always
            // the current binding value (a reassigned `let` updates the cell in
            // place), so both `const` and `let` are safe to inline.
            if let Some((cell, _is_const)) = self.global_lexicals.get(name).copied() {
                instr.global_lex_cell = Some(cell.offset());
            }
        }
    }

    /// Replay an object literal's shape transitions from the empty root,
    /// returning the hidden class the literal's object ends up in after its
    /// `keys` are defined in order with default data attributes — the same
    /// transitions `set_property` performs at construction time. `None` if any
    /// transition fails to allocate.
    pub(crate) fn shape_after_keys(&mut self, keys: &[&str]) -> Option<object::ShapeHandle> {
        let roots = self.collect_runtime_roots_without_shape_runtime();
        let mut shape = self.shape_runtime.root();
        let flags = object::PropertyFlags::data_default();
        for &key in keys {
            let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
                for &slot in &roots {
                    visitor(slot);
                }
            };
            shape = self
                .shape_runtime
                .child_with_roots(
                    &mut self.gc_heap,
                    shape,
                    key,
                    flags,
                    false,
                    &mut external_visit,
                )
                .ok()?;
        }
        Some(shape)
    }

    /// Bake object-literal allocation plans into `view`: for each `NewObject`
    /// that begins a single-block run of `DefineDataProperty` with constant
    /// string keys, record the literal's final hidden class (computed by
    /// replaying its shape transitions) so the optimizing tier can allocate it in
    /// one shaped allocation instead of per-property shape walks.
    ///
    /// A plan is recorded only for a literal that is provably the simple case:
    /// every property is a distinct, non-`__proto__`, non-index string key whose
    /// slot lands in definition order, the whole run is straight-line (no
    /// branch), and there are at most four properties (the register-passed value
    /// budget). Anything else is left unplanned and lowers on the baseline.
    pub(crate) fn bake_object_literals(
        &mut self,
        view: &mut jit::JitCompileSnapshot,
        context: &ExecutionContext,
    ) {
        use otter_bytecode::{Op, Operand};
        const MAX_PROPS: usize = 4;
        let code_block = std::sync::Arc::clone(&view.code_block);
        let reg_op =
            |instr: &jit::JitInstructionMetadata, i: usize| match code_block.operand(instr, i) {
                Some(Operand::Register(r)) => Some(r),
                _ => None,
            };
        let const_op =
            |instr: &jit::JitInstructionMetadata, i: usize| match code_block.operand(instr, i) {
                Some(Operand::ConstIndex(n)) => Some(n),
                _ => None,
            };
        let uses_reg = |instr: &jit::JitInstructionMetadata, reg: u16| {
            code_block
                .operand_view(instr)
                .iter()
                .any(|o| matches!(o, Operand::Register(r) if r == reg))
        };

        let fid = view.code_block.id;
        let mut plans: Vec<(usize, jit::ObjectLiteralPlan)> = Vec::new();
        let instrs = &view.instructions;
        for i in 0..instrs.len() {
            if instrs[i].op != Op::NewObject {
                continue;
            }
            let Some(obj_reg) = reg_op(&instrs[i], 0) else {
                continue;
            };
            let mut defines: Vec<jit::ObjectLiteralProp> = Vec::new();
            let mut key_pcs: Vec<u32> = Vec::new();
            let mut keys: Vec<String> = Vec::new();
            let mut ok = true;
            let mut j = i + 1;
            while j < instrs.len() {
                let instr = &instrs[j];
                // A `__proto__`-free data property defined on this literal:
                // `LoadString key` immediately followed by `DefineDataProperty
                // obj, key, value`.
                if instr.op == Op::DefineDataProperty && reg_op(instr, 0) == Some(obj_reg) {
                    let (Some(key_reg), Some(val_reg)) = (reg_op(instr, 1), reg_op(instr, 2))
                    else {
                        ok = false;
                        break;
                    };
                    if j == 0 {
                        ok = false;
                        break;
                    }
                    let prev = &instrs[j - 1];
                    if prev.op != Op::LoadString || reg_op(prev, 0) != Some(key_reg) {
                        ok = false;
                        break;
                    }
                    let Some(kidx) = const_op(prev, 1) else {
                        ok = false;
                        break;
                    };
                    let Some(key) = context.string_constant_str_for_function(fid, kidx) else {
                        ok = false;
                        break;
                    };
                    // `__proto__` mutates the prototype, not a slot, so its shape
                    // does not match a replayed data transition. A duplicate key
                    // would not advance the slot count. Both abort the plan.
                    if key == "__proto__" || keys.iter().any(|k| k == key) {
                        ok = false;
                        break;
                    }
                    keys.push(key.to_string());
                    defines.push(jit::ObjectLiteralProp {
                        define_pc: instr.byte_pc,
                        value_reg: val_reg,
                    });
                    key_pcs.push(prev.byte_pc);
                    j += 1;
                    continue;
                }
                // The literal ends when the object first escapes (is read), and a
                // branch would split the run across blocks — neither is foldable.
                if uses_reg(instr, obj_reg) {
                    break;
                }
                if matches!(instr.op, Op::Jump | Op::JumpIfTrue | Op::JumpIfFalse) {
                    ok = false;
                    break;
                }
                j += 1;
            }
            if !ok || keys.is_empty() || keys.len() > MAX_PROPS {
                continue;
            }
            let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
            let Some(shape) = self.shape_after_keys(&key_refs) else {
                continue;
            };
            // The replayed shape must assign each key the slot matching its
            // definition order, so the bulk slot-initializer fills values that
            // line up with the literal's value list.
            let mut valid = true;
            for (idx, key) in key_refs.iter().enumerate() {
                if self.shape_offset_of(shape, key) != Some(idx as u32) {
                    valid = false;
                    break;
                }
            }
            if !valid {
                continue;
            }
            plans.push((
                i,
                jit::ObjectLiteralPlan {
                    obj_reg,
                    shape_offset: shape.offset(),
                    defines,
                    key_pcs,
                },
            ));
        }
        for (i, plan) in plans {
            view.instructions[i].object_literal = Some(plan);
        }
    }

    /// JIT bridge: allocate an object literal directly in its baked final hidden
    /// class for the optimizing tier's `AllocObjectLiteral`. `values_bits` are
    /// the NaN-boxed property values in slot order (passed by value from compiled
    /// code). The values are rooted across the allocation, then bulk-written into
    /// the shaped object's slots (generational write barriers applied in
    /// [`object::initialize_shaped_data_slots`]), and `%Object.prototype%` is
    /// installed — matching the interpreter's per-property construction result.
    ///
    /// # Errors
    /// Propagates allocation failure (heap-cap `RangeError` / cage exhaustion).
    pub fn jit_runtime_alloc_object_literal(
        &mut self,
        stack: &mut HoltStack,
        frame_index: usize,
        dst: u16,
        shape_offset: u32,
        values_bits: &[u64],
    ) -> Result<(), VmError> {
        // Decode the boxed values out of the transient JIT argument registers
        // before any allocation can run a scavenge.
        let values: smallvec::SmallVec<[Value; 4]> =
            values_bits.iter().map(|&b| Value::from_bits(b)).collect();
        // SAFETY: `shape_offset` is a compressed `Gc<ShapeBody>` offset baked from
        // a live shape during compilation; shapes are interned for the isolate's
        // life, so the offset still decompresses to that shape. The cage is
        // initialised (we are executing JS).
        let shape = unsafe { object::ShapeHandle::from_offset(shape_offset) };
        let roots = self.collect_allocation_roots(stack);
        let obj = {
            let mut external_visit = |visitor: &mut dyn FnMut(*mut RawGc)| {
                for &slot in &roots {
                    visitor(slot);
                }
                for value in values.iter() {
                    value.trace_value_slots(visitor);
                }
            };
            object::alloc_object_with_shape_roots(&mut self.gc_heap, shape, &mut external_visit)
                .map_err(VmError::from)?
        };
        // The slow alloc path above may have scavenged; read the relocated realm
        // prototype handle afterwards (mirrors `run_new_object_reg`).
        if let Some(proto) = self.object_prototype_object_opt() {
            object::set_prototype(obj, &mut self.gc_heap, Some(proto));
        }
        object::initialize_shaped_data_slots(obj, &mut self.gc_heap, &values);
        let frame = &mut stack[frame_index];
        write_register(frame, dst, Value::object(obj))?;
        Ok(())
    }

    /// Record a `Mono`/`Poly` observation for one `Op::Call` site, the feedback
    /// the baseline reads to decide whether to inline a tiny leaf callee. First
    /// sighting at a site is `Mono(callee)`; a later sighting of the *same*
    /// callee is a no-op; any *different* callee promotes the site to `Poly`
    /// permanently. Only reached when a JIT hook is installed (the `Op::Call`
    /// arm gates it), so interpreter-only execution pays nothing.
    pub(crate) fn note_call_target(&mut self, caller_fid: u32, call_byte_pc: u32, callee_fid: u32) {
        use std::collections::hash_map::Entry;
        let newly_mono = match self
            .jit_call_site_feedback
            .entry((caller_fid, call_byte_pc))
        {
            Entry::Vacant(slot) => {
                slot.insert(CallTargetFeedback::Mono(callee_fid));
                true
            }
            Entry::Occupied(mut slot) => {
                if let CallTargetFeedback::Mono(seen) = *slot.get()
                    && seen != callee_fid
                {
                    slot.insert(CallTargetFeedback::Poly);
                }
                false
            }
        };
        if newly_mono {
            self.evict_compiled_for_reopt(caller_fid);
        }
    }

    /// Whether a method-call site's feedback has already saturated to
    /// `Megamorphic`. Once it has, further [`Self::note_method_target`]
    /// observations are no-ops, so a caller can skip the receiver/prototype
    /// shape walk that only exists to build the `MethodSite` argument — the hot
    /// path for a megamorphic site (e.g. one `arr[i].run()` over many classes).
    pub(crate) fn method_site_feedback_saturated(
        &self,
        caller_fid: u32,
        call_byte_pc: u32,
    ) -> bool {
        matches!(
            self.jit_method_site_feedback
                .get(&(caller_fid, call_byte_pc)),
            Some(MethodCallFeedback::Megamorphic)
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
        caller_fid: u32,
        call_byte_pc: u32,
        method_fid: u32,
        site: MethodSite,
    ) {
        use std::collections::hash_map::Entry;
        let new_target = PolyMethodTarget {
            method_fid,
            recv_shape: site.recv_shape,
            proto_chain: site.proto_chain,
            method_value_byte: site.method_value_byte,
            hits: 1,
        };
        match self
            .jit_method_site_feedback
            .entry((caller_fid, call_byte_pc))
        {
            Entry::Vacant(slot) => {
                slot.insert(MethodCallFeedback::Mono {
                    method_fid,
                    recv_shape: site.recv_shape,
                    proto_chain: site.proto_chain,
                    method_value_byte: site.method_value_byte,
                });
            }
            Entry::Occupied(mut slot) => match slot.get_mut() {
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
                        *slot.get_mut() = MethodCallFeedback::Poly(Box::new(targets));
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
                        *slot.get_mut() = MethodCallFeedback::Megamorphic;
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
        for (&(caller_fid, byte_pc), state) in &self.jit_call_site_feedback {
            if caller_fid != fid {
                continue;
            }
            let CallTargetFeedback::Mono(callee_fid) = *state else {
                if trace {
                    eprintln!("[otter-jit] inline callee skip fid {fid} pc {byte_pc}: poly");
                }
                continue;
            };
            let Some(callee) = context.exec_function(callee_fid) else {
                if trace {
                    eprintln!(
                        "[otter-jit] inline callee skip fid {fid} pc {byte_pc}: missing callee {callee_fid}"
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
                        "[otter-jit] inline callee skip fid {fid} pc {byte_pc}: ineligible callee {callee_fid} flags gen={} async={} async_gen={} args={} rest={} eval={} derived={} makes_fn={}",
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
                        "[otter-jit] inline callee skip fid {fid} pc {byte_pc}: missing view {callee_fid}"
                    );
                }
                continue;
            };
            if trace {
                eprintln!(
                    "[otter-jit] inline callee bake fid {fid} pc {byte_pc}: callee {callee_fid}"
                );
            }
            view.inline_callees.insert(
                byte_pc,
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
            byte_pc: u32,
            targets: SmallVec<[PolyMethodTarget; MAX_POLY_METHOD_TARGETS]>,
        }
        let method_sites: Vec<PolySnapshot> = self
            .jit_method_site_feedback
            .iter()
            .filter_map(|(&(caller_fid, byte_pc), state)| {
                if caller_fid != fid {
                    return None;
                }
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
                        Some(PolySnapshot { byte_pc, targets })
                    }
                    MethodCallFeedback::Poly(observed) => {
                        let mut targets = (**observed).clone();
                        // Most-frequent target first: the common receiver shape
                        // then hits the shortest guard chain.
                        targets.sort_by_key(|t| std::cmp::Reverse(t.hits));
                        Some(PolySnapshot { byte_pc, targets })
                    }
                    MethodCallFeedback::Megamorphic => None,
                }
            })
            .collect();
        for snap in method_sites {
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
                        .insert(snap.byte_pc, baked.pop().unwrap());
                }
                // Two or more: emit the guarded inline chain.
                _ => {
                    view.inline_poly_methods.insert(snap.byte_pc, baked);
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
        let mut method_view = context.jit_compile_snapshot(target.method_fid)?;
        // Populate each body op's site feedback so the inliner can lower it: the
        // property `(shape, slot)` for a non-receiver access, and the arithmetic /
        // element-load kind so a body `|`/`&`/`x[i]` lowers to a typed node
        // instead of declining on empty feedback.
        self.bake_property_feedback(&mut method_view);
        self.bake_arith_feedback(&mut method_view, target.method_fid);
        self.bake_element_load_kind(&mut method_view, target.method_fid);
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
            let name_operand = match instr.op {
                Op::LoadProperty => 2,
                Op::StoreProperty => 1,
                _ => continue,
            };
            let otter_bytecode::Operand::ConstIndex(name_idx) =
                method_view.code_block.operand(instr, name_operand)?
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
            let (shape_off, value_byte) = instr.property_feedback?;
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
                if instr.op != Op::CallMethodValue {
                    continue;
                }
                if let Some(MethodCallFeedback::Mono {
                    method_fid,
                    recv_shape,
                    proto_chain,
                    method_value_byte,
                }) = self
                    .jit_method_site_feedback
                    .get(&(target.method_fid, instr.byte_pc))
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
            if instr.op != Op::CallMethodValue {
                continue;
            }
            let Some(site) = instr.property_ic_site else {
                continue;
            };
            if let Some(feedback) = self.jit_array_method_feedback(site) {
                view.array_methods.insert(instr.byte_pc, feedback);
            }
        }
    }

    pub(crate) fn bake_primitive_method_guards(&self, view: &mut jit::JitCompileSnapshot) {
        for instr in &view.instructions {
            if instr.op != Op::CallMethodValue {
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
            if instr.op != Op::CallMethodValue {
                continue;
            }
            let Some(site) = instr.property_ic_site else {
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
            if instr.op != Op::CallMethodValue {
                continue;
            }
            let Some(site) = instr.property_ic_site else {
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
