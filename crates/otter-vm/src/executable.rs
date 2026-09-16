//! Authoritative immutable CodeBlock execution representation.
//!
//! `otter-bytecode` owns the compiler/debug DTO shape. The VM owns this
//! compact view so hot dispatch reads opcodes, verified operand words, and
//! named-property IC sites from one record while byte coordinates stay cold.
//!
//! # Contents
//! - [`ExecutableModuleBuilder`] — transient builder over retained bytecode
//!   admission proofs.
//! - [`ExecutableModule`] — VM-owned frozen function table.
//! - [`CodeBlock`] — one verified function body: immutable wordcode/control
//!   flow plus dense advisory feedback cells keyed by logical PC.
//! - [`CodeBlockInstruction`] — the sole VM execution record: opcode, verified
//!   operand words, canonical PC, and VM-local IC metadata.
//!
//! # Invariants
//! - `frame.pc` is the dense instruction index into `CodeBlock::code`.
//! - Serialized byte coordinates live only in the CodeBlock's cold metadata;
//!   hot instruction records carry the canonical logical instruction PC.
//! - Cold byte PCs are a one-way logical-PC source/profiling map; execution has
//!   no byte-PC-to-instruction reverse lookup.
//! - Operand payloads are untagged 32-bit words already covered by the
//!   [`VerifiedBytecodeModule`] proof. Typed hot accessors read them without
//!   repeating schema or function-table lookup.
//! - Executable construction consumes retained layouts and register-window
//!   sizes; it never re-runs hostile-input validation.
//! - Up to four operand words live in the execution record. Any longer
//!   instruction uses the CodeBlock-owned overflow table; no parallel active
//!   wordcode array or per-instruction reference count remains.
//! - Branch-class `Imm32` operands hold instruction-index deltas relative to
//!   the next instruction. `NO_HANDLER_OFFSET` is preserved for absent
//!   try-handler slots by the serialized verifier.
//! - Named property IC sites receive dense VM-local ids during build; the
//!   bytecode JSON dump stays unchanged.
//! - A tier-neutral [`crate::feedback::FeedbackVector`] owns both dense
//!   instruction cells and their monotonic material-transition epoch.
//!
//! # See also
//! - [`crate::execution_context`]
//! - [`otter_bytecode::Instruction`]

#[path = "code_block_cfg.rs"]
pub(crate) mod code_block_cfg;

use otter_bytecode::{
    ArgumentBindingStorage, ArgumentsObjectKind, Function, FunctionCode, FunctionCodeBuilder, Op,
    Operand, SpanEntry, VerifiedBytecodeModule, VerifiedFunction,
    encoding::{measure_wordcode_function, translate_spans_to_byte_pcs},
};
use std::sync::Arc;

use code_block_cfg::{CodeBlockControlFlow, CodeBlockExceptionRegion};

pub(crate) const NO_PROPERTY_IC_SITE: u32 = u32::MAX;

/// Transient builder for [`ExecutableModule`].
///
/// The builder owns dense IC-site assignment while the VM creates an
/// [`crate::ExecutionContext`]. Dispatch receives only the frozen
/// [`ExecutableModule`] produced by [`Self::freeze`].
#[derive(Debug, Default)]
pub(crate) struct ExecutableModuleBuilder {
    functions: Vec<Arc<CodeBlock>>,
    next_property_ic_site: u32,
}

impl ExecutableModuleBuilder {
    /// Build a transient executable view from the compiler/debug module DTO.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn from_bytecode(module: &otter_bytecode::BytecodeModule) -> Self {
        let verified = VerifiedBytecodeModule::new(module.clone())
            .expect("executable test fixture must be valid bytecode");
        Self::from_verified_bytecode_with_ic_base(&verified, 0)
    }

    /// Build a transient executable view from a retained admission proof.
    /// Dense property-IC site ids start at `property_ic_base`, keeping sites
    /// globally unique across chunks linked into one interpreter.
    #[must_use]
    pub(crate) fn from_verified_bytecode_with_ic_base(
        verified: &VerifiedBytecodeModule,
        property_ic_base: u32,
    ) -> Self {
        let module = verified.module();
        let mut builder = Self {
            functions: Vec::with_capacity(module.functions.len()),
            next_property_ic_site: property_ic_base,
        };
        for (index, function) in module.functions.iter().enumerate() {
            let proof = verified
                .function(index)
                .expect("verified carrier has one proof per function");
            builder.push_function(function, proof, &module.module);
        }
        builder
    }

    fn push_function(&mut self, function: &Function, proof: &VerifiedFunction, module_url: &str) {
        let function = Arc::new(CodeBlock::from_verified_bytecode(
            function,
            proof,
            module_url,
            &mut self.next_property_ic_site,
        ));
        self.functions.push(function);
    }

    /// Seal mutable build buffers into the VM-owned frozen execution product.
    #[must_use]
    pub(crate) fn freeze(self) -> ExecutableModule {
        ExecutableModule {
            functions: self.functions.into_boxed_slice(),
            property_ic_site_end: self.next_property_ic_site,
        }
    }
}

/// VM-owned executable view of a bytecode module.
#[derive(Debug, Clone)]
pub(crate) struct ExecutableModule {
    functions: Box<[Arc<CodeBlock>]>,
    property_ic_site_end: u32,
}

/// Stable directory entry mapping a globally dense method site id back to its
/// owning CodeBlock method marker.
#[derive(Debug, Clone)]
pub(crate) struct FeedbackSlotAddress {
    code_block: Arc<CodeBlock>,
    instruction_index: usize,
}

impl FeedbackSlotAddress {
    #[must_use]
    pub(crate) fn is_method(&self) -> bool {
        self.code_block
            .feedback
            .is_method_slot(self.instruction_index)
    }
}

impl ExecutableModule {
    /// Build a frozen execution view from the compiler/debug module DTO.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn from_bytecode(module: &otter_bytecode::BytecodeModule) -> Self {
        ExecutableModuleBuilder::from_bytecode(module).freeze()
    }

    /// Build a frozen execution view from a retained admission proof. Dense
    /// property-IC site ids start at `property_ic_base`.
    #[must_use]
    pub(crate) fn from_verified_bytecode_with_ic_base(
        verified: &VerifiedBytecodeModule,
        property_ic_base: u32,
    ) -> Self {
        ExecutableModuleBuilder::from_verified_bytecode_with_ic_base(verified, property_ic_base)
            .freeze()
    }

    /// Function-table lookup by chunk-local function index.
    #[must_use]
    pub(crate) fn function(&self, local_index: u32) -> Option<&CodeBlock> {
        self.functions.get(local_index as usize).map(Arc::as_ref)
    }

    /// Shared immutable CodeBlock handle for native compilation.
    #[must_use]
    pub(crate) fn function_arc(&self, local_index: u32) -> Option<Arc<CodeBlock>> {
        self.functions.get(local_index as usize).cloned()
    }

    /// One past the highest dense named-property IC site id in this
    /// module (equals the site count when the IC base is zero).
    #[must_use]
    pub(crate) const fn property_ic_site_end(&self) -> u32 {
        self.property_ic_site_end
    }

    /// Heap bytes this execution view retains for the owning chunk's
    /// lifetime: the function table plus every CodeBlock body. Saturating, so
    /// a saturated total still exceeds any real budget and fails admission
    /// closed.
    #[must_use]
    pub(crate) fn retained_bytes(&self) -> u64 {
        let mut total = std::mem::size_of_val::<[Arc<CodeBlock>]>(&self.functions) as u64;
        for code_block in &self.functions {
            total = total
                .saturating_add(std::mem::size_of::<CodeBlock>() as u64)
                .saturating_add(code_block.retained_bytes());
        }
        total
    }

    /// Build directory entries for method sites in this chunk.
    pub(crate) fn feedback_slot_addresses(&self) -> Vec<(usize, FeedbackSlotAddress)> {
        let mut slots = Vec::new();
        for code_block in &self.functions {
            for (instruction_index, instruction) in code_block.code.iter().enumerate() {
                if code_block.op(instruction) == Op::CallMethodValue
                    && let Some(site) = instruction.property_ic_site()
                {
                    slots.push((
                        site,
                        FeedbackSlotAddress {
                            code_block: Arc::clone(code_block),
                            instruction_index,
                        },
                    ));
                }
            }
        }
        slots
    }

    pub(crate) fn trace_property_ic_roots(&self, visitor: &mut otter_gc::raw::SlotVisitor<'_>) {
        for code_block in &self.functions {
            code_block.feedback.trace_property_roots(visitor);
        }
    }

    pub(crate) fn property_ic_stats(&self) -> crate::property_ic::PropertyIcStats {
        let mut total = crate::property_ic::PropertyIcStats::default();
        for code_block in &self.functions {
            let stats = code_block.feedback.property_stats();
            total.load_hits = total.load_hits.saturating_add(stats.load_hits);
            total.load_misses = total.load_misses.saturating_add(stats.load_misses);
            total.load_installs = total.load_installs.saturating_add(stats.load_installs);
            total.load_disables = total.load_disables.saturating_add(stats.load_disables);
            total.store_hits = total.store_hits.saturating_add(stats.store_hits);
            total.store_misses = total.store_misses.saturating_add(stats.store_misses);
            total.store_installs = total.store_installs.saturating_add(stats.store_installs);
            total.store_disables = total.store_disables.saturating_add(stats.store_disables);
        }
        total
    }

    #[cfg(test)]
    pub(crate) fn polymorphic_property_count(
        &self,
        kind: crate::property_ic::PropertyIcKind,
    ) -> usize {
        self.functions
            .iter()
            .map(|code_block| code_block.feedback.polymorphic_property_count(kind))
            .sum()
    }

    pub(crate) fn property_ic_snapshots(&self) -> Vec<crate::inspect::IcSiteSnapshot> {
        let mut out = Vec::new();
        for code_block in &self.functions {
            for (instruction_index, instruction) in code_block.code.iter().enumerate() {
                let Some(site) = instruction.property_ic_site() else {
                    continue;
                };
                let (kind, inspect_kind) = match code_block.op(instruction) {
                    Op::LoadProperty | Op::CallMethodValue => (
                        crate::property_ic::PropertyIcKind::Load,
                        crate::inspect::IcSiteKind::Load,
                    ),
                    Op::StoreProperty | Op::StorePropertyStrict => (
                        crate::property_ic::PropertyIcKind::Store,
                        crate::inspect::IcSiteKind::Store,
                    ),
                    _ => continue,
                };
                let Some(slot) = code_block.property_feedback_at(instruction_index, kind) else {
                    continue;
                };
                out.push(crate::inspect::IcSiteSnapshot {
                    site_index: site as u32,
                    kind: inspect_kind,
                    state: slot.snapshot_state(),
                });
            }
        }
        out
    }
}

impl CodeBlock {
    /// Heap bytes this code block retains beyond `size_of::<Self>()`: the
    /// instruction stream, overflow operand words, control-flow and span
    /// tables, feedback vector, and annotation-hint tables.
    #[must_use]
    pub(crate) fn retained_bytes(&self) -> u64 {
        (std::mem::size_of_val::<[ExecMappedArgumentBinding]>(&self.mapped_argument_bindings)
            as u64)
            .saturating_add(self.module_url.len() as u64)
            .saturating_add(std::mem::size_of_val::<[ExecDirectEvalBinding]>(
                &self.direct_eval_bindings,
            ) as u64)
            .saturating_add(
                self.direct_eval_bindings
                    .iter()
                    .fold(0u64, |total, binding| {
                        total.saturating_add(binding.name.len() as u64)
                    }),
            )
            .saturating_add(std::mem::size_of_val::<[CodeBlockInstruction]>(&self.code) as u64)
            .saturating_add(std::mem::size_of_val::<[u32]>(&self.overflow_operand_words) as u64)
            .saturating_add(self.control_flow.retained_bytes())
            .saturating_add(self.feedback.retained_bytes())
            .saturating_add(std::mem::size_of_val::<[u32]>(&self.byte_pcs) as u64)
            .saturating_add(std::mem::size_of_val::<[SpanEntry]>(&self.byte_spans) as u64)
            .saturating_add(std::mem::size_of_val::<[u64]>(&self.number_hints) as u64)
            .saturating_add(std::mem::size_of_val::<[(u32, u32)]>(&self.class_hints) as u64)
    }

    /// Build JIT feedback/layout metadata over this exact immutable CodeBlock.
    #[must_use]
    pub(crate) fn jit_compile_snapshot(self: &Arc<Self>) -> crate::jit::JitCompileSnapshot {
        let gc_header_bytes = otter_gc::header::HEADER_SIZE as u32;
        crate::jit::JitCompileSnapshot {
            code_block: Arc::clone(self),
            derived_constructor: self.is_derived_constructor,
            // Baked by `Interpreter::compile_jit_function`, which holds the
            // cage base and the live property-IC tables.
            cage_base: 0,
            // Baked alongside `cage_base` by `compile_jit_function`; the
            // all-zero default is never read because the emitter gates inline
            // element access on `cage_base != 0`.
            array_layout: crate::jit::JitArrayLayout::default(),
            element_accesses: rustc_hash::FxHashMap::default(),
            string_layout: crate::jit::JitStringLayout::default(),
            // `#[repr(C)]` constant: offset from the decompressed object
            // pointer to its shape handle for native CacheIR guards.
            object_shape_byte: otter_gc::header::HEADER_SIZE as u32
                + crate::object::OBJECT_BODY_SHAPE_OFFSET as u32,
            object_dictionary_shape_id_byte: otter_gc::header::HEADER_SIZE as u32
                + crate::object::OBJECT_BODY_DICTIONARY_SHAPE_ID_OFFSET as u32,
            object_values_ptr_byte: otter_gc::header::HEADER_SIZE as u32
                + crate::object::OBJECT_BODY_VALUES_PTR_OFFSET as u32,
            object_inline_values_byte: otter_gc::header::HEADER_SIZE as u32
                + crate::object::OBJECT_BODY_INLINE_VALUES_OFFSET as u32,
            object_slab_handle_byte: otter_gc::header::HEADER_SIZE as u32
                + crate::object::OBJECT_BODY_SLAB_HANDLE_OFFSET as u32,
            object_slab_len_byte: otter_gc::header::HEADER_SIZE as u32
                + crate::object::OBJECT_BODY_SLAB_LEN_OFFSET as u32,
            object_inline_slot_cap: crate::object::INLINE_SLOT_CAP as u32,
            object_slab_capacity_byte: otter_gc::header::HEADER_SIZE as u32
                + crate::object::slot_slab::SLOT_SLAB_CAPACITY_OFFSET as u32,
            object_extensible_byte: otter_gc::header::HEADER_SIZE as u32
                + crate::object::OBJECT_BODY_EXTENSIBLE_OFFSET as u32,
            object_chain_link_opaque_byte: otter_gc::header::HEADER_SIZE as u32
                + crate::object::OBJECT_BODY_CHAIN_LINK_OPAQUE_OFFSET as u32,
            object_shape_cache_mode_byte: otter_gc::header::HEADER_SIZE as u32
                + crate::object::OBJECT_BODY_SHAPE_CACHE_MODE_OFFSET as u32,
            object_shape_cache_fast: crate::object::SHAPE_CACHE_MODE_FAST,
            object_slot_attrs_overridden_byte: otter_gc::header::HEADER_SIZE as u32
                + crate::object::OBJECT_BODY_SLOT_ATTRS_OVERRIDDEN_OFFSET as u32,
            object_exotic_handle_byte: otter_gc::header::HEADER_SIZE as u32
                + crate::object::OBJECT_BODY_EXOTIC_HANDLE_OFFSET as u32,
            object_cell_bytes: crate::object::OBJECT_BODY_CELL_BYTES as u32,
            gc_barrier: crate::jit::JitGcBarrierLayout {
                header_flags_byte: otter_gc::header::HEADER_FLAGS_BYTE_OFFSET as u32,
                young_flag: otter_gc::header::GENERATION_YOUNG_FLAG as u32,
                remembered_flag: otter_gc::header::REMEMBERED_FLAG as u32,
            },
            jit_proto_byte: otter_gc::header::HEADER_SIZE as u32
                + crate::object::OBJECT_BODY_JIT_PROTO_OFFSET as u32,
            closure_call_layout: crate::jit::JitClosureCallLayout {
                function_id_byte: gc_header_bytes
                    + crate::closure::CLOSURE_BODY_FUNCTION_ID_OFFSET as u32,
                flags_byte: gc_header_bytes + crate::closure::CLOSURE_BODY_CALL_FLAGS_OFFSET as u32,
                upvalue_base_byte: gc_header_bytes
                    + crate::closure::CLOSURE_BODY_UPVALUE_BASE_OFFSET as u32,
                upvalue_count_byte: gc_header_bytes
                    + crate::closure::CLOSURE_BODY_UPVALUE_COUNT_OFFSET as u32,
                eval_env_byte: gc_header_bytes
                    + crate::closure::CLOSURE_BODY_EVAL_ENV_OFFSET as u32,
                bound_this_byte: gc_header_bytes
                    + crate::closure::CLOSURE_BODY_BOUND_THIS_OFFSET as u32,
                bound_new_target_byte: gc_header_bytes
                    + crate::closure::CLOSURE_BODY_BOUND_NEW_TARGET_OFFSET as u32,
                bound_this_flag: crate::closure::CLOSURE_CALL_FLAG_BOUND_THIS,
                bound_new_target_flag: crate::closure::CLOSURE_CALL_FLAG_BOUND_NEW_TARGET,
                runtime_setup_flags: crate::closure::CLOSURE_CALL_RUNTIME_SETUP_FLAGS,
                own_props_byte: gc_header_bytes
                    + crate::closure::CLOSURE_BODY_OWN_PROPS_OFFSET as u32,
                prototype_shape_byte: gc_header_bytes
                    + crate::closure::CLOSURE_BODY_PROTOTYPE_SHAPE_OFFSET as u32,
                prototype_slot_byte: gc_header_bytes
                    + crate::closure::CLOSURE_BODY_PROTOTYPE_SLOT_OFFSET as u32,
                learned_instance_fields_byte: gc_header_bytes
                    + crate::closure::CLOSURE_BODY_LEARNED_INSTANCE_FIELDS_OFFSET as u32,
                last_instance_byte: gc_header_bytes
                    + crate::closure::CLOSURE_BODY_LAST_INSTANCE_OFFSET as u32,
            },
            class_constructor_layout: crate::jit::JitClassConstructorLayout {
                type_tag: crate::class_constructor::CLASS_CONSTRUCTOR_BODY_TYPE_TAG,
                callable_byte: gc_header_bytes
                    + crate::class_constructor::CLASS_CONSTRUCTOR_BODY_CTOR_OFFSET as u32,
                super_constructor_byte: gc_header_bytes
                    + crate::class_constructor::CLASS_CONSTRUCTOR_BODY_CTOR_PROTO_OFFSET as u32,
                prototype_byte: gc_header_bytes
                    + crate::class_constructor::CLASS_CONSTRUCTOR_BODY_PROTOTYPE_OFFSET as u32,
            },
            primitive_cell_type_tags: [
                crate::string::JS_STRING_BODY_TYPE_TAG,
                crate::symbol::SYMBOL_BODY_TYPE_TAG,
                crate::bigint::BIG_INT_BODY_TYPE_TAG,
            ],
            upvalue_value_byte: otter_gc::header::HEADER_SIZE as u32
                + std::mem::offset_of!(crate::upvalue::UpvalueCellBody, value) as u32,
            collection_layout: crate::jit::JitCollectionLayout {
                map_type_tag: crate::collections::MAP_BODY_TYPE_TAG,
                set_type_tag: crate::collections::SET_BODY_TYPE_TAG,
                guard_flags_byte: otter_gc::header::HEADER_SIZE as u32
                    + crate::collections::MAP_BODY_JIT_GUARD_FLAGS_OFFSET as u32,
                native_function_type_tag: crate::native_function::NATIVE_FUNCTION_BODY_TYPE_TAG,
                map_table: crate::jit::JitMapTableLayout {
                    map_table_byte: gc_header_bytes
                        + crate::collections::MAP_BODY_TABLE_OFFSET as u32,
                    table_type_tag: crate::collections::MAP_TABLE_BODY_TYPE_TAG,
                    table_len_byte: gc_header_bytes
                        + crate::collections::table::ORDERED_TABLE_LEN_OFFSET as u32,
                    table_bucket_mask_byte: gc_header_bytes
                        + crate::collections::table::ORDERED_TABLE_BUCKET_MASK_OFFSET as u32,
                    table_buckets_byte: gc_header_bytes
                        + crate::collections::table::ORDERED_TABLE_BODY_SIZE as u32,
                    entry_size: crate::collections::MAP_ENTRY_SIZE as u32,
                    entry_key_byte: crate::collections::MAP_ENTRY_KEY_OFFSET as u32,
                    entry_value_byte: crate::collections::MAP_ENTRY_VALUE_OFFSET as u32,
                    entry_next_byte: crate::collections::MAP_ENTRY_NEXT_OFFSET as u32,
                    entry_flags_byte: crate::collections::MAP_ENTRY_FLAGS_OFFSET as u32,
                    entry_live_flag: crate::collections::MAP_ENTRY_LIVE_FLAG,
                    number_hash_tag: crate::collections::MAP_NUMBER_HASH_TAG,
                    fx_hash_multiplier: crate::collections::MAP_FX_HASH_MULTIPLIER,
                    hash_avalanche_1: crate::collections::MAP_HASH_AVALANCHE_1,
                    hash_avalanche_2: crate::collections::MAP_HASH_AVALANCHE_2,
                },
            },
            native_ref_byte: otter_gc::header::HEADER_SIZE as u32
                + crate::native_function::NATIVE_FUNCTION_BODY_NATIVE_REF_OFFSET as u32,
            instructions: self
                .code
                .iter()
                .enumerate()
                .map(|(index, _)| crate::jit::JitInstructionMetadata {
                    instruction_index: index as u32,
                    byte_pc: self.byte_pcs[index],
                    // Resolved by `ExecutionContext::jit_compile_snapshot`, which
                    // can map a `MakeFunction` constant index to its target id.
                    make_self: false,
                    // Resolved by `ExecutionContext::jit_compile_snapshot`, which
                    // can inspect constant strings without exposing them to the
                    // external JIT crate.
                    load_array_length: false,
                    method_hint: crate::jit::JitMethodHint::None,
                    // Resolved by `ExecutionContext::jit_compile_snapshot`, which
                    // can read the number-constant pool for a `LoadNumber`.
                    load_number: None,
                    // A call attempt is recorded before callable/property
                    // resolution, so absence means the branch truly stayed
                    // cold rather than merely throwing before target feedback.
                    call_attempted: self
                        .feedback_at(index)
                        .is_some_and(crate::feedback::InstructionFeedback::call_attempted),
                    // A site that has never executed falls back to the
                    // compiler's TypeScript annotation, which is enough for the
                    // optimizing tier to pick a guarded numeric lowering
                    // instead of treating the site as unreachable. Any
                    // recorded observation supersedes the annotation.
                    arith_feedback: match self
                        .feedback_at(index)
                        .map_or(0, crate::feedback::InstructionFeedback::arith_bits)
                    {
                        0 if self.has_number_hint(index) => {
                            crate::feedback::ArithFeedback::number_annotation_seed()
                        }
                        bits => crate::feedback::ArithFeedback::from_bits(bits),
                    },
                })
                .collect(),
            // Baked by `Interpreter::bake_global_lexical_loads`, which owns the
            // live global declarative record. Raw snapshots carry no GC cell
            // identity.
            global_lexical_loads: rustc_hash::FxHashMap::default(),
            // Baked by `Interpreter::bake_string_constant_cells`, which owns
            // the address-stable traced literal cells. Raw snapshots carry no
            // process-local cell identity.
            string_constant_cells: rustc_hash::FxHashMap::default(),
            // Baked by `Interpreter::bake_global_lexical_loads`, which owns
            // the live global declarative record and global object.
            global_object_loads: rustc_hash::FxHashMap::default(),
            // Baked from the authoritative typed `Op::Call` distribution by
            // `Interpreter::bake_inline_callees`.
            static_native_calls: rustc_hash::FxHashMap::default(),
            // Baked by `Interpreter::bake_inline_callees` (it holds the live
            // per-site feedback and can resolve callee bodies); the raw snapshot
            // carries none.
            direct_callees: rustc_hash::FxHashMap::default(),
            direct_constructs: rustc_hash::FxHashMap::default(),
            direct_methods: rustc_hash::FxHashMap::default(),
            inline_callees: rustc_hash::FxHashMap::default(),
            inline_methods: rustc_hash::FxHashMap::default(),
            inline_poly_methods: rustc_hash::FxHashMap::default(),
            guarded_method_calls: rustc_hash::FxHashMap::default(),
            property_programs: rustc_hash::FxHashMap::default(),
            binding_hit_proofs: rustc_hash::FxHashMap::default(),
            constructor_field_transitions: rustc_hash::FxHashMap::default(),
            optimized_exit_reasons: std::collections::BTreeMap::new(),
            safepoints: rustc_hash::FxHashMap::default(),
        }
    }

    /// Construct the authoritative executable body used by a backend unit-test
    /// snapshot. Kept crate-private so production callers can only obtain a
    /// snapshot from a verified compiler `CodeBlock`.
    #[doc(hidden)]
    #[must_use]
    pub(crate) fn jit_test_stub(
        id: u32,
        param_count: u16,
        register_count: u16,
        instructions: &[crate::jit::JitTestInstruction],
    ) -> Arc<Self> {
        let mut wordcode_builder = FunctionCodeBuilder::new();
        for instr in instructions {
            wordcode_builder.push(instr.op, &instr.operands);
        }
        let wordcode = wordcode_builder.finish();
        let bytecode_byte_len = measure_wordcode_function(&wordcode)
            .expect("test bytecode size fits the schema")
            .total_bytes;
        let byte_pcs: Vec<_> = instructions.iter().map(|instr| instr.byte_pc).collect();
        let mut overflow_operand_words = Vec::new();
        let code: Vec<_> = instructions
            .iter()
            .enumerate()
            .map(|(word_index, instr)| {
                CodeBlockInstruction::from_wordcode(
                    &wordcode,
                    word_index,
                    id,
                    instr.instruction_pc,
                    NO_PROPERTY_IC_SITE,
                    &mut overflow_operand_words,
                )
            })
            .collect();
        let feedback = crate::feedback::FeedbackVector::for_instruction_ops(
            instructions.iter().map(|instruction| instruction.op),
        );
        let control_flow = CodeBlockControlFlow::from_verified_wordcode(&wordcode);
        Arc::new(Self {
            id,
            param_count,
            register_count,
            own_upvalue_count: 0,
            inherited_upvalue_count: 0,
            is_strict: false,
            is_arrow: false,
            is_method: false,
            has_rest: false,
            is_async: false,
            is_generator: false,
            is_async_generator: false,
            is_derived_constructor: false,
            makes_function: false,
            needs_arguments: false,
            arguments_object_kind: ArgumentsObjectKind::Unmapped,
            mapped_argument_bindings: Box::new([]),
            is_module: false,
            module_url: Box::<str>::from(""),
            direct_eval_bindings: Box::new([]),
            eval_sites: Box::new([]),
            contains_direct_eval: false,
            code: code.into_boxed_slice(),
            overflow_operand_words: overflow_operand_words.into_boxed_slice(),
            bytecode_byte_len,
            control_flow,
            feedback,
            byte_pcs: byte_pcs.into_boxed_slice(),
            byte_spans: Box::new([]),
            number_hints: Box::new([]),
            class_hints: Box::new([]),
        })
    }

    /// Byte-offset source-map entries, sorted by `pc`. Empty when the
    /// underlying [`Function::spans`] is empty.
    #[must_use]
    pub(crate) fn byte_spans(&self) -> &[SpanEntry] {
        &self.byte_spans
    }

    /// Fetch an instruction by its dense `code` index.
    #[must_use]
    pub(crate) fn instr_at_index(&self, index: usize) -> Option<&CodeBlockInstruction> {
        self.code.get(index)
    }

    /// Dense feedback cell at canonical logical `index`.
    #[must_use]
    pub(crate) fn feedback_at(
        &self,
        index: usize,
    ) -> Option<&crate::feedback::InstructionFeedback> {
        self.feedback.cell(index)
    }

    /// CodeBlock-wide version of material feedback transitions.
    ///
    /// The baseline tier ignores this telemetry; optimizing promotion samples
    /// it to require stable feedback before compilation.
    #[must_use]
    pub fn feedback_epoch(&self) -> u32 {
        self.feedback.epoch()
    }

    /// Pair one dense feedback cell with this CodeBlock's transition epoch.
    #[must_use]
    pub(crate) fn feedback_recorder_at(
        &self,
        index: usize,
    ) -> Option<crate::feedback::InstructionFeedbackRecorder<'_>> {
        self.feedback.recorder(index)
    }

    #[must_use]
    pub(crate) fn property_feedback_at(
        &self,
        index: usize,
        kind: crate::property_ic::PropertyIcKind,
    ) -> Option<crate::feedback::PropertyFeedbackSlot<'_>> {
        self.feedback.property_slot(index, kind)
    }

    /// Advance the shared epoch for isolate-owned feedback that changed
    /// outside the dense CodeBlock cells.
    pub(crate) fn bump_feedback_epoch(&self) {
        self.feedback.bump_epoch();
    }

    /// Bounded ordinary-call distribution at one canonical instruction.
    #[must_use]
    pub(crate) fn call_distribution_at(
        &self,
        index: usize,
    ) -> Option<crate::feedback::CallSiteDistribution> {
        self.feedback.call_slot(index)?.distribution()
    }

    /// Record one ordinary bytecode target through the feedback facade.
    #[cfg(test)]
    pub(crate) fn record_call_feedback(
        &self,
        instruction_index: usize,
        callee_fid: u32,
    ) -> crate::feedback::CallTargetTransition {
        self.feedback.record_call(
            instruction_index,
            crate::feedback::OrdinaryCallTarget::Bytecode(callee_fid),
        )
    }

    /// Record one typed ordinary-call target through the feedback facade.
    pub(crate) fn record_call_target_feedback(
        &self,
        instruction_index: usize,
        target: crate::feedback::OrdinaryCallTarget,
    ) -> crate::feedback::CallTargetTransition {
        self.feedback.record_call(instruction_index, target)
    }

    /// Cold serialized byte PC for one logical instruction index.
    #[must_use]
    pub(crate) fn instruction_byte_pc(&self, index: usize) -> Option<u32> {
        self.byte_pcs.get(index).copied()
    }

    /// Operands in schema declaration order.
    #[cfg(test)]
    #[must_use]
    pub fn operands(&self, instr: &CodeBlockInstruction) -> smallvec::SmallVec<[Operand; 4]> {
        (0..self.operand_count(instr))
            .map(|index| {
                self.operand(instr, index)
                    .expect("verified CodeBlock operand must decode")
            })
            .collect()
    }

    /// Opcode from the active VM execution record.
    #[must_use]
    pub fn op(&self, instr: &CodeBlockInstruction) -> Op {
        instr.op
    }

    /// Opcode at one canonical instruction index.
    #[must_use]
    pub fn op_at(&self, index: usize) -> Option<Op> {
        self.code.get(index).map(|instruction| instruction.op)
    }

    /// Exact encoded byte length of this function's bytecode stream.
    #[must_use]
    pub const fn bytecode_byte_len(&self) -> u32 {
        self.bytecode_byte_len
    }

    /// Source module URL carried by this function.
    #[must_use]
    pub fn module_url(&self) -> &str {
        &self.module_url
    }

    /// Sorted logical PCs beginning basic blocks in this function.
    #[must_use]
    pub fn block_starts(&self) -> &[u32] {
        self.control_flow.block_starts()
    }

    /// Borrow the immutable logical control-flow tables for this function.
    #[must_use]
    pub fn control_flow(&self) -> crate::CodeBlockControlFlowView<'_> {
        crate::CodeBlockControlFlowView::new(&self.control_flow)
    }

    /// Sorted logical PCs targeted by backwards normal-flow edges.
    #[must_use]
    pub fn loop_headers(&self) -> &[u32] {
        self.control_flow.loop_headers()
    }

    /// Last logical backedge PC for a loop header.
    #[must_use]
    pub(crate) fn loop_latch(&self, header_pc: u32) -> Option<u32> {
        self.control_flow.loop_latch(header_pc)
    }

    /// Resolved handlers installed by an `EnterTry` instruction.
    #[must_use]
    pub(crate) fn exception_region(&self, enter_pc: u32) -> Option<CodeBlockExceptionRegion> {
        self.control_flow.exception_region(enter_pc)
    }

    /// Number of schema-typed operands on this instruction.
    #[must_use]
    pub fn operand_count(&self, instr: &CodeBlockInstruction) -> usize {
        instr.operand_count as usize
    }

    /// Whether every operand word lives in the instruction record.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn operands_are_inline(&self, instr: &CodeBlockInstruction) -> bool {
        instr.operand_count as usize <= instr.inline_operand_words.len()
    }

    /// Borrowed schema-decoded operand view with no materialisation.
    #[must_use]
    pub const fn operand_view<'a>(&'a self, instr: &'a CodeBlockInstruction) -> OperandView<'a> {
        OperandView {
            source: OperandViewSource::Execution {
                code_block: self,
                instr,
            },
        }
    }

    /// All verified operand words of one instruction, in schema order.
    ///
    /// Variadic call sites read their whole argument run through this slice so
    /// per-argument decoding collapses to one indexed load.
    #[must_use]
    pub(crate) fn operand_words<'a>(&'a self, instr: &'a CodeBlockInstruction) -> &'a [u32] {
        let count = instr.operand_count as usize;
        if count <= instr.inline_operand_words.len() {
            &instr.inline_operand_words[..count]
        } else {
            let base = instr.inline_operand_words[0] as usize;
            self.overflow_operand_words
                .get(base..base + count)
                .unwrap_or(&[])
        }
    }

    /// Verified operand word `index` of one instruction.
    ///
    /// Construction verifies operand kinds and count against the opcode schema,
    /// so dispatch reads a declared operand directly instead of re-deriving its
    /// kind and re-checking presence on every executed instruction.
    #[inline]
    #[must_use]
    pub(crate) fn word(&self, instr: &CodeBlockInstruction, index: usize) -> u32 {
        debug_assert!(
            index < instr.operand_count as usize,
            "declared operand index in range"
        );
        if (instr.operand_count as usize) <= instr.inline_operand_words.len() {
            instr.inline_operand_words[index]
        } else {
            self.overflow_operand_words[instr.inline_operand_words[0] as usize + index]
        }
    }

    /// Register operand `index`.
    #[inline]
    #[must_use]
    pub(crate) fn reg(&self, instr: &CodeBlockInstruction, index: usize) -> u16 {
        self.word(instr, index) as u16
    }

    /// One schema-typed operand.
    #[must_use]
    pub fn operand(&self, instr: &CodeBlockInstruction, index: usize) -> Option<Operand> {
        let word = self.operand_word(instr, index)?;
        let kind = otter_bytecode::opcode_schema::operand_kind_at(instr.op, index)?;
        otter_bytecode::opcode_schema::decode_operand_word(kind, word)
    }

    /// Decode one register operand.
    #[must_use]
    pub fn register(&self, instr: &CodeBlockInstruction, index: usize) -> Option<u16> {
        let word = self.operand_word(instr, index)?;
        debug_assert_eq!(
            otter_bytecode::opcode_schema::operand_kind_at(instr.op, index),
            Some(otter_bytecode::opcode_schema::OperandKind::Register)
        );
        u16::try_from(word).ok()
    }

    /// Decode the common `dst, lhs, rhs` register triple.
    #[must_use]
    pub fn register3(&self, instr: &CodeBlockInstruction) -> Option<(u16, u16, u16)> {
        Some((
            self.register(instr, 0)?,
            self.register(instr, 1)?,
            self.register(instr, 2)?,
        ))
    }

    /// Decode one constant-pool index operand.
    #[must_use]
    pub fn const_index(&self, instr: &CodeBlockInstruction, index: usize) -> Option<u32> {
        let word = self.operand_word(instr, index)?;
        debug_assert_eq!(
            otter_bytecode::opcode_schema::operand_kind_at(instr.op, index),
            Some(otter_bytecode::opcode_schema::OperandKind::ConstIndex)
        );
        Some(word)
    }

    /// Decode one signed immediate operand.
    #[must_use]
    pub fn imm32(&self, instr: &CodeBlockInstruction, index: usize) -> Option<i32> {
        let word = self.operand_word(instr, index)?;
        debug_assert_eq!(
            otter_bytecode::opcode_schema::operand_kind_at(instr.op, index),
            Some(otter_bytecode::opcode_schema::OperandKind::Imm32)
        );
        Some(word as i32)
    }

    #[inline]
    fn operand_word(&self, instr: &CodeBlockInstruction, index: usize) -> Option<u32> {
        if index >= instr.operand_count as usize {
            return None;
        }
        if instr.operand_count as usize <= instr.inline_operand_words.len() {
            return instr.inline_operand_words.get(index).copied();
        }
        self.overflow_operand_words
            .get(instr.inline_operand_words[0] as usize + index)
            .copied()
    }
}

/// Borrowed access to one instruction's schema-typed operand words.
///
/// The view is copyable and decodes individual words on demand. It never owns
/// or materialises an `Operand` collection.
#[derive(Clone, Copy)]
pub struct OperandView<'a> {
    source: OperandViewSource<'a>,
}

#[derive(Clone, Copy)]
enum OperandViewSource<'a> {
    Execution {
        code_block: &'a CodeBlock,
        instr: &'a CodeBlockInstruction,
    },
    #[cfg(test)]
    Decoded(&'a [Operand]),
}

impl<'a> OperandView<'a> {
    /// Number of operands declared by the verified instruction.
    #[must_use]
    pub fn len(self) -> usize {
        match self.source {
            OperandViewSource::Execution { code_block, instr } => code_block.operand_count(instr),
            #[cfg(test)]
            OperandViewSource::Decoded(decoded) => decoded.len(),
        }
    }

    /// Whether this instruction has no operands.
    #[must_use]
    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    /// Decode one schema-typed operand.
    #[must_use]
    pub fn get(self, index: usize) -> Option<Operand> {
        match self.source {
            OperandViewSource::Execution { code_block, instr } => code_block.operand(instr, index),
            #[cfg(test)]
            OperandViewSource::Decoded(decoded) => decoded.get(index).copied(),
        }
    }

    /// Decode the first operand.
    #[must_use]
    pub fn first(self) -> Option<Operand> {
        self.get(0)
    }

    /// Iterate over decoded operands without allocating a collection.
    pub fn iter(self) -> impl ExactSizeIterator<Item = Operand> + 'a {
        (0..self.len()).map(move |index| {
            self.get(index)
                .expect("verified CodeBlock operand must decode")
        })
    }
}

/// Copyable operand source accepted by semantic helpers during the wordcode
/// migration. Production dispatch supplies [`OperandView`]; borrowed decoded
/// slices remain available to focused unit tests and cold tooling.
pub(crate) trait OperandSource: Copy {
    /// Decode one operand by position.
    fn get(self, index: usize) -> Option<Operand>;

    /// Decode the first operand.
    fn first(self) -> Option<Operand> {
        self.get(0)
    }
}

impl OperandSource for OperandView<'_> {
    fn get(self, index: usize) -> Option<Operand> {
        self.get(index)
    }
}

impl OperandSource for &[Operand] {
    fn get(self, index: usize) -> Option<Operand> {
        <[Operand]>::get(self, index).copied()
    }
}

impl<const N: usize> OperandSource for &[Operand; N] {
    fn get(self, index: usize) -> Option<Operand> {
        self.as_slice().get(index).copied()
    }
}

impl std::fmt::Debug for OperandView<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

#[cfg(test)]
impl<'a> From<&'a [Operand]> for OperandView<'a> {
    fn from(decoded: &'a [Operand]) -> Self {
        Self {
            source: OperandViewSource::Decoded(decoded),
        }
    }
}

#[cfg(test)]
impl<'a, const N: usize> From<&'a [Operand; N]> for OperandView<'a> {
    fn from(decoded: &'a [Operand; N]) -> Self {
        Self::from(decoded.as_slice())
    }
}

#[cfg(test)]
impl<'a> From<&'a Vec<Operand>> for OperandView<'a> {
    fn from(decoded: &'a Vec<Operand>) -> Self {
        Self::from(decoded.as_slice())
    }
}

/// One immutable, schema-verified executable function body.
///
/// Construction verifies the compiler DTO directly in logical instruction-index
/// coordinates, then builds schema-typed words. Cold byte-PC layout is computed
/// without materialising or decoding the self-describing serialized stream.
#[derive(Debug)]
pub struct CodeBlock {
    /// Global VM function id (chunk base + local table index).
    pub id: u32,
    /// Number of parameter registers at the start of the frame.
    pub param_count: u16,
    /// Total register window size: params + locals + scratch.
    pub register_count: u16,
    /// Number of fresh upvalue cells owned by each frame.
    pub(crate) own_upvalue_count: u16,
    /// Exact number of closure-owned cells appended after fresh frame cells.
    pub(crate) inherited_upvalue_count: u16,
    /// `true` when this function uses strict-mode call semantics.
    pub is_strict: bool,
    /// `true` when this function is an arrow function.
    pub(crate) is_arrow: bool,
    /// `true` when this function is a MethodDefinition body (class
    /// or object-literal method / accessor) — never a constructor,
    /// carries no implicit `prototype` property.
    pub(crate) is_method: bool,
    /// `true` when this function declares a rest parameter.
    pub(crate) has_rest: bool,
    /// `true` when this function is async.
    pub is_async: bool,
    /// `true` when this function is a generator.
    pub is_generator: bool,
    /// `true` when this function is an async generator.
    pub is_async_generator: bool,
    /// `true` when this function is a derived-class constructor whose
    /// `this` is bound by `super(...)` (§10.2.2). Frame setup starts
    /// it in the TDZ.
    pub(crate) is_derived_constructor: bool,
    /// `true` when this function body contains an `Op::MakeFunction` or
    /// `Op::MakeClosure`. The per-instance SELF binding (cold-frame
    /// `callee_closure`) is read only by those opcodes, so the call dispatcher
    /// records the closure (and acquires a cold frame for it) only when this is
    /// set — leaf functions and most callbacks skip it entirely.
    pub(crate) makes_function: bool,
    /// `true` when this function body needs an `arguments` object.
    pub(crate) needs_arguments: bool,
    /// Arguments object shape requested by the compiler.
    pub(crate) arguments_object_kind: ArgumentsObjectKind,
    /// Compact mapped-arguments bindings without debug-only formal names.
    pub(crate) mapped_argument_bindings: Box<[ExecMappedArgumentBinding]>,
    /// `true` when this function is an ES module body.
    pub(crate) is_module: bool,
    /// Source module URL carried by frames for module resolution.
    pub(crate) module_url: Box<str>,
    /// §19.2.1.3 — name → own-upvalue table for direct eval. On a
    /// function containing a direct eval call site this lists every
    /// function-scope binding; on a compiled eval `<main>` it lists
    /// the new var-scoped bindings the body introduced.
    pub(crate) direct_eval_bindings: Box<[ExecDirectEvalBinding]>,
    /// Per-`Op::Eval`-site caller-scope refinements (block-scope
    /// bindings visible at each direct-eval site, all `inner`).
    pub(crate) eval_sites: Box<[Box<[ExecDirectEvalBinding]>]>,
    /// §19.2.1.1 `inFunction` signal for `Op::Eval` — `true` when
    /// this function contains a direct eval call site (the binding
    /// table may still be empty).
    pub(crate) contains_direct_eval: bool,
    /// Sole hot instruction stream indexed directly by the frame's canonical PC.
    pub code: Box<[CodeBlockInstruction]>,
    /// Operand words for uncommon instructions wider than four operands.
    overflow_operand_words: Box<[u32]>,
    /// Exact encoded byte length computed with the authoritative layout.
    bytecode_byte_len: u32,
    /// Precomputed logical-PC block, loop, and exception-region tables.
    control_flow: CodeBlockControlFlow,
    /// Tier-neutral advisory feedback parallel to `code`, including its single
    /// monotonic transition epoch, shared without hash lookup.
    feedback: crate::feedback::FeedbackVector,
    /// Cold serialized byte PCs parallel to `code`.
    byte_pcs: Box<[u32]>,
    /// Source-map entries with `pc` expressed as a byte offset into the
    /// encoded stream. Empty when the underlying [`Function::spans`] is empty.
    pub(crate) byte_spans: Box<[SpanEntry]>,
    /// One bit per instruction index: the compiler saw TypeScript `number` on
    /// both operands of this site. Empty when the body carries no annotations.
    ///
    /// Read only when the site's feedback cell is still empty, so a recorded
    /// profile always wins. Seeding an unwarmed site lets the optimizing tier
    /// lower it under its ordinary numeric guard instead of refusing it for
    /// lack of a profile; a wrong annotation trips that guard once.
    pub(crate) number_hints: Box<[u64]>,
    /// Property sites whose receiver is annotated with a locally declared
    /// class, as `(instruction index, class constructor function id)` sorted by
    /// index. Empty when the body carries no such annotation.
    ///
    /// Read only when the site's inline cache is still empty, so a recorded
    /// shape always wins. Seeding lets a tier resolve the access on first
    /// compile instead of refusing it for lack of a profile; a wrong annotation
    /// misses the guard the site already emits.
    pub(crate) class_hints: Box<[(u32, u32)]>,
}

impl CodeBlock {
    fn from_verified_bytecode(
        function: &Function,
        proof: &VerifiedFunction,
        module_url: &str,
        next_property_ic_site: &mut u32,
    ) -> Self {
        let register_count = proof.register_count();
        let code_byte_len = proof.layout().total_bytes;
        let instr_to_byte_pc = proof.layout().instr_to_byte_pc.clone();
        let control_flow = CodeBlockControlFlow::from_verified_wordcode(&function.code);
        let mut overflow_operand_words = Vec::new();
        let code = function
            .code
            .iter()
            .enumerate()
            .map(|(idx, instr)| {
                let property_ic_site = match instr.op {
                    // `CallMethodValue` shares the load-IC table: a prototype
                    // method is a data slot on the prototype, so its resolution
                    // is cached by receiver shape exactly like a `LoadProperty`.
                    Op::LoadProperty
                    | Op::StoreProperty
                    | Op::StorePropertyStrict
                    | Op::CallMethodValue => {
                        let site = *next_property_ic_site;
                        *next_property_ic_site = next_property_ic_site
                            .checked_add(1)
                            .expect("property IC site table exceeds u32");
                        site
                    }
                    _ => NO_PROPERTY_IC_SITE,
                };
                CodeBlockInstruction::from_wordcode(
                    &function.code,
                    idx,
                    function.id,
                    idx as u32,
                    property_ic_site,
                    &mut overflow_operand_words,
                )
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let feedback = crate::feedback::FeedbackVector::for_instruction_ops(
            function.code.iter().map(|instruction| instruction.op),
        );
        let number_hints = if function.number_hint_sites.is_empty() {
            Box::new([]) as Box<[u64]>
        } else {
            let mut bits = vec![0u64; code.len().div_ceil(64)];
            for &site in &function.number_hint_sites {
                let index = site as usize;
                if index < code.len() {
                    bits[index / 64] |= 1 << (index % 64);
                }
            }
            bits.into_boxed_slice()
        };
        let mut class_hints: Vec<(u32, u32)> = function
            .class_hint_sites
            .iter()
            .filter(|site| (site.pc as usize) < code.len())
            .map(|site| (site.pc, site.class_function_id))
            .collect();
        class_hints.sort_unstable_by_key(|&(pc, _)| pc);
        class_hints.dedup_by_key(|&mut (pc, _)| pc);
        let class_hints = class_hints.into_boxed_slice();
        let mapped_argument_bindings = function
            .mapped_argument_bindings
            .iter()
            .map(|binding| ExecMappedArgumentBinding {
                argument_index: binding.argument_index,
                storage: binding.storage,
            })
            .collect();
        let byte_spans =
            translate_spans_to_byte_pcs(&function.spans, &instr_to_byte_pc, code_byte_len)
                .into_boxed_slice();
        // The per-instance SELF binding (`callee_closure` in the cold frame) is
        // consumed *only* by `Op::MakeFunction` / `Op::MakeClosure` resolving the
        // running function id. A body with neither opcode can never read it, so
        // the call dispatcher skips recording the closure — and thus the cold
        // frame acquire/release entirely — for such functions. Conservative: a
        // body that makes *any* closure keeps it (self-reference can't be ruled
        // out without resolving operands), which is the rare case in hot code.
        let makes_function = function
            .code
            .iter()
            .any(|instr| matches!(instr.op, Op::MakeFunction | Op::MakeClosure));
        Self {
            id: function.id,
            makes_function,
            param_count: function.param_count,
            register_count,
            own_upvalue_count: function.own_upvalue_count,
            inherited_upvalue_count: function.inherited_upvalue_count,
            is_strict: function.is_strict,
            is_arrow: function.is_arrow,
            is_method: function.is_method,
            has_rest: function.has_rest,
            is_derived_constructor: function.is_derived_constructor,
            is_async: function.is_async,
            is_generator: function.is_generator,
            is_async_generator: function.is_async_generator,
            needs_arguments: function.needs_arguments,
            arguments_object_kind: function.arguments_object_kind,
            mapped_argument_bindings,
            is_module: function.is_module,
            module_url: if function.module_url.is_empty() {
                module_url.into()
            } else {
                function.module_url.clone().into_boxed_str()
            },
            direct_eval_bindings: function
                .direct_eval_bindings
                .iter()
                .map(exec_direct_eval_binding)
                .collect(),
            eval_sites: function
                .eval_sites
                .iter()
                .map(|site| site.iter().map(exec_direct_eval_binding).collect())
                .collect(),
            contains_direct_eval: function.contains_direct_eval,
            code,
            overflow_operand_words: overflow_operand_words.into_boxed_slice(),
            bytecode_byte_len: code_byte_len,
            control_flow,
            feedback,
            byte_pcs: instr_to_byte_pc,
            byte_spans,
            number_hints,
            class_hints,
        }
    }

    /// `true` when the compiler marked this instruction's operands as
    /// statically `number`. Advisory — see [`Self::number_hints`].
    /// Constructor function id of the class annotated on this instruction's
    /// receiver. Advisory — see [`Self::class_hints`].
    pub(crate) fn class_hint(&self, instruction_index: usize) -> Option<u32> {
        let index = u32::try_from(instruction_index).ok()?;
        self.class_hints
            .binary_search_by_key(&index, |&(pc, _)| pc)
            .ok()
            .map(|found| self.class_hints[found].1)
    }

    fn has_number_hint(&self, instruction_index: usize) -> bool {
        let word = instruction_index / 64;
        self.number_hints
            .get(word)
            .is_some_and(|bits| bits & (1 << (instruction_index % 64)) != 0)
    }
}

/// One direct-eval caller binding: name → own-upvalue cell index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExecDirectEvalBinding {
    /// Source-level binding name.
    pub(crate) name: Box<str>,
    /// Own-upvalue cell index inside the owning function's frame.
    pub(crate) upvalue: u16,
    /// `true` for `let` / `const` / `class` bindings.
    pub(crate) lexical: bool,
    /// Passthrough capture from an enclosing function (§19.2.1.3 —
    /// readable, but not part of the caller's varEnv).
    pub(crate) captured: bool,
    /// `true` for a `const` / `class` caller binding — an eval-body
    /// assignment throws `TypeError` in every mode (§13.3.1).
    pub(crate) is_const: bool,
    /// `true` for a named function expression's self-name binding —
    /// an eval-body assignment throws `TypeError` in strict mode only
    /// (§10.2.11, §9.1.1.1.5).
    pub(crate) fn_self_name: bool,
    /// Block-scope binding between the variable environment and the
    /// eval site (§19.2.1.3, §B.3.5) — shadows the baseline table and
    /// eval-environment records; a body `var` never re-binds it.
    pub(crate) inner: bool,
    /// Formal-parameter binding (or the implicit `arguments` object):
    /// part of the function environment a parameter-initializer eval
    /// already sees; body var/function bindings are not (§10.2.11).
    pub(crate) param: bool,
    /// Deletable eval-introduced caller binding (§19.2.1.3
    /// CreateMutableBinding with deletable = true).
    pub(crate) deletable: bool,
    /// 1-based lexical scope depth inside the owning function (function
    /// scope is `1`), used to order the binding against a `with` object
    /// environment (§9.1.1.2.1).
    pub(crate) scope_depth: u16,
}

fn exec_direct_eval_binding(binding: &otter_bytecode::DirectEvalBinding) -> ExecDirectEvalBinding {
    ExecDirectEvalBinding {
        name: binding.name.clone().into_boxed_str(),
        upvalue: binding.upvalue,
        lexical: binding.lexical,
        captured: binding.captured,
        is_const: binding.is_const,
        fn_self_name: binding.fn_self_name,
        inner: binding.inner,
        param: binding.param,
        deletable: binding.deletable,
        scope_depth: binding.scope_depth,
    }
}

/// Compact mapped-arguments alias entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ExecMappedArgumentBinding {
    /// Argument object index.
    pub(crate) argument_index: u16,
    /// Storage backing the parameter binding.
    pub(crate) storage: ArgumentBindingStorage,
}

/// Dense VM execution record built once from verified compiler wordcode.
#[repr(C)]
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CodeBlockInstruction {
    /// Canonical dense instruction index used by interpreter and JIT CFG.
    pub instruction_pc: u32,
    /// Dense module-local property IC site id for named property ops.
    property_ic_site: u32,
    /// Owning CodeBlock id for cold context-resolving helpers.
    code_block_id: u32,
    /// Common operand payloads, already schema-verified.
    inline_operand_words: [u32; 4],
    /// Opcode dispatched directly by the interpreter and read by JIT planning.
    op: Op,
    /// Number of verified operand words.
    operand_count: u8,
    /// Runtime-budget cost of executing this instruction once. Metering runs on
    /// every dispatched instruction, so the opcode's fixed weight is resolved
    /// when the record is built rather than re-derived from the opcode each
    /// tick.
    reductions: u8,
    reserved: u8,
}

const _: [(); 32] = [(); std::mem::size_of::<CodeBlockInstruction>()];
const _: [(); 4] = [(); std::mem::align_of::<CodeBlockInstruction>()];

impl CodeBlockInstruction {
    fn from_wordcode(
        code: &FunctionCode,
        word_index: usize,
        code_block_id: u32,
        instruction_pc: u32,
        property_ic_site: u32,
        overflow_operand_words: &mut Vec<u32>,
    ) -> Self {
        let source = &code[word_index];
        let operand_count = source.operand_count();
        let mut inline_operand_words = [0; 4];
        if operand_count <= inline_operand_words.len() {
            for (index, slot) in inline_operand_words
                .iter_mut()
                .take(operand_count)
                .enumerate()
            {
                *slot = operand_payload(
                    code.operand(source, index)
                        .expect("verified wordcode operand must decode"),
                );
            }
        } else {
            let offset = u32::try_from(overflow_operand_words.len())
                .expect("executable operand table exceeds u32");
            inline_operand_words[0] = offset;
            overflow_operand_words.extend((0..operand_count).map(|index| {
                operand_payload(
                    code.operand(source, index)
                        .expect("verified wordcode operand must decode"),
                )
            }));
        }
        Self {
            instruction_pc,
            property_ic_site,
            code_block_id,
            inline_operand_words,
            op: source.op,
            operand_count: u8::try_from(operand_count)
                .expect("executable operand count exceeds u8"),
            reductions: crate::work_budget::opcode_work_units(source.op),
            reserved: 0,
        }
    }

    /// Runtime-budget cost of one execution of this instruction.
    #[inline]
    #[must_use]
    pub(crate) const fn reductions(&self) -> u64 {
        self.reductions as u64
    }

    /// Owning CodeBlock identity for cold context-resolving helpers.
    #[must_use]
    pub(crate) const fn code_block_id(&self) -> u32 {
        self.code_block_id
    }

    /// Dense property IC site index for named property opcodes.
    #[must_use]
    pub fn property_ic_site(&self) -> Option<usize> {
        (self.property_ic_site != NO_PROPERTY_IC_SITE).then_some(self.property_ic_site as usize)
    }

    /// Operand word `index` of an instruction whose schema shape is fixed at no
    /// more than four operands.
    ///
    /// Such an instruction always carries every operand in the record itself, so
    /// dispatch reads the word with no count test, no overflow-table load, and
    /// no owning-CodeBlock access. Opcodes with a variadic tail or a wider fixed
    /// shape must use the CodeBlock accessors, which resolve the overflow table.
    #[inline]
    #[must_use]
    const fn inline_word(&self, index: usize) -> u32 {
        debug_assert!(
            index < self.operand_count as usize,
            "declared operand index in range"
        );
        debug_assert!(
            self.operand_count as usize <= self.inline_operand_words.len(),
            "narrow fixed-shape operands are inline"
        );
        self.inline_operand_words[index]
    }

    /// Register operand `index` of a narrow fixed-shape instruction.
    #[inline]
    #[must_use]
    pub(crate) const fn reg(&self, index: usize) -> u16 {
        self.inline_word(index) as u16
    }

    /// The common `dst, lhs, rhs` register triple.
    #[inline]
    #[must_use]
    pub(crate) const fn reg3(&self) -> (u16, u16, u16) {
        (self.reg(0), self.reg(1), self.reg(2))
    }

    /// Constant-pool index operand `index` of a narrow fixed-shape instruction.
    #[inline]
    #[must_use]
    pub(crate) const fn const_word(&self, index: usize) -> u32 {
        self.inline_word(index)
    }

    /// Signed immediate operand `index` of a narrow fixed-shape instruction.
    #[inline]
    #[must_use]
    pub(crate) const fn imm(&self, index: usize) -> i32 {
        self.inline_word(index) as i32
    }
}

#[inline]
const fn operand_payload(operand: Operand) -> u32 {
    match operand {
        Operand::Register(value) => value as u32,
        Operand::ConstIndex(value) => value,
        Operand::Imm32(value) => value as u32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Value;
    use otter_bytecode::{BytecodeModule, Instruction, SourceKind};

    fn function(mut code: Vec<Instruction>) -> Function {
        // Size the register window to the operands the test actually uses, so
        // the build-time register verifier accepts these hand-written bodies.
        let mut max_register = 0u32;
        for instruction in &code {
            for index in 0..instruction.operands.len() {
                if otter_bytecode::opcode_schema::register_access_at(instruction.op, index)
                    == otter_bytecode::opcode_schema::RegisterAccess::None
                {
                    continue;
                }
                let register = match instruction.operands[index] {
                    Operand::Register(value) => u32::from(value),
                    Operand::Imm32(value) => u32::try_from(value).unwrap_or(0),
                    Operand::ConstIndex(value) => value,
                };
                max_register = max_register.max(register + 1);
            }
        }
        code.push(Instruction {
            pc: code.len() as u32,
            op: Op::ReturnUndefined,
            operands: Vec::new(),
        });
        Function {
            id: 0,
            name: "exec-test".to_string(),
            locals: u16::try_from(max_register).expect("test register index fits the window"),
            code: code.into(),
            ..Function::default()
        }
    }

    fn module(function: Function) -> BytecodeModule {
        BytecodeModule {
            module: "exec-test".to_string(),
            template_sites: Vec::new(),
            source_kind: SourceKind::JavaScript,
            functions: vec![function],
            constants: (0..8)
                .map(|index| otter_bytecode::Constant::String {
                    utf16: format!("name-{index}").encode_utf16().collect(),
                })
                .collect(),
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
            function_source: None,
        }
    }

    #[test]
    fn number_annotation_seeds_only_unrecorded_arithmetic() {
        let mut hinted = function(vec![
            Instruction {
                pc: 0,
                op: Op::Add,
                operands: vec![
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::Register(2),
                ],
            },
            Instruction {
                pc: 1,
                op: Op::Mul,
                operands: vec![
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::Register(2),
                ],
            },
        ]);
        hinted.number_hint_sites = vec![0];
        let executable = ExecutableModule::from_bytecode(&module(hinted));
        let code_block = executable.function_arc(0).expect("function");

        // The unrecorded hinted site reads as numeric, so the optimizing tier
        // lowers it under a guard instead of treating it as unreachable.
        let snapshot = code_block.jit_compile_snapshot();
        assert!(snapshot.feedback_at(0).is_numeric_only());
        assert!(!snapshot.feedback_at(0).is_int32_only());
        // An unhinted site keeps reporting that it never executed.
        assert!(snapshot.feedback_at(1).is_unseen());

        // A real observation supersedes the annotation, including the
        // narrower `int32` case the annotation cannot express.
        code_block
            .feedback_at(0)
            .expect("arith cell")
            .record_arith(Value::number_i32(1), Value::number_i32(2));
        let snapshot = code_block.jit_compile_snapshot();
        assert!(snapshot.feedback_at(0).is_int32_only());
    }

    #[test]
    fn jit_snapshot_publishes_typed_closure_call_layout() {
        let executable = ExecutableModule::from_bytecode(&module(function(Vec::new())));
        let function = executable.function_arc(0).expect("function");
        let snapshot = function.jit_compile_snapshot();
        let gc_header_bytes = otter_gc::header::HEADER_SIZE as u32;

        assert_eq!(
            snapshot.closure_call_layout,
            crate::jit::JitClosureCallLayout {
                function_id_byte: gc_header_bytes
                    + crate::closure::CLOSURE_BODY_FUNCTION_ID_OFFSET as u32,
                flags_byte: gc_header_bytes + crate::closure::CLOSURE_BODY_CALL_FLAGS_OFFSET as u32,
                upvalue_base_byte: gc_header_bytes
                    + crate::closure::CLOSURE_BODY_UPVALUE_BASE_OFFSET as u32,
                upvalue_count_byte: gc_header_bytes
                    + crate::closure::CLOSURE_BODY_UPVALUE_COUNT_OFFSET as u32,
                eval_env_byte: gc_header_bytes
                    + crate::closure::CLOSURE_BODY_EVAL_ENV_OFFSET as u32,
                bound_this_byte: gc_header_bytes
                    + crate::closure::CLOSURE_BODY_BOUND_THIS_OFFSET as u32,
                bound_new_target_byte: gc_header_bytes
                    + crate::closure::CLOSURE_BODY_BOUND_NEW_TARGET_OFFSET as u32,
                bound_this_flag: crate::closure::CLOSURE_CALL_FLAG_BOUND_THIS,
                bound_new_target_flag: crate::closure::CLOSURE_CALL_FLAG_BOUND_NEW_TARGET,
                runtime_setup_flags: crate::closure::CLOSURE_CALL_RUNTIME_SETUP_FLAGS,
                own_props_byte: gc_header_bytes
                    + crate::closure::CLOSURE_BODY_OWN_PROPS_OFFSET as u32,
                prototype_shape_byte: gc_header_bytes
                    + crate::closure::CLOSURE_BODY_PROTOTYPE_SHAPE_OFFSET as u32,
                prototype_slot_byte: gc_header_bytes
                    + crate::closure::CLOSURE_BODY_PROTOTYPE_SLOT_OFFSET as u32,
                learned_instance_fields_byte: gc_header_bytes
                    + crate::closure::CLOSURE_BODY_LEARNED_INSTANCE_FIELDS_OFFSET as u32,
                last_instance_byte: gc_header_bytes
                    + crate::closure::CLOSURE_BODY_LAST_INSTANCE_OFFSET as u32,
            }
        );
    }

    #[test]
    fn jit_snapshot_publishes_property_semantic_guard_layout() {
        let executable = ExecutableModule::from_bytecode(&module(function(Vec::new())));
        let function = executable.function_arc(0).expect("function");
        let snapshot = function.jit_compile_snapshot();
        let header = otter_gc::header::HEADER_SIZE as u32;

        assert_eq!(
            snapshot.object_shape_cache_mode_byte,
            header + crate::object::OBJECT_BODY_SHAPE_CACHE_MODE_OFFSET as u32
        );
        assert_eq!(
            snapshot.object_shape_cache_fast,
            crate::object::SHAPE_CACHE_MODE_FAST
        );
        assert_eq!(
            snapshot.object_slot_attrs_overridden_byte,
            header + crate::object::OBJECT_BODY_SLOT_ATTRS_OVERRIDDEN_OFFSET as u32
        );
        assert_eq!(
            snapshot.object_exotic_handle_byte,
            header + crate::object::OBJECT_BODY_EXOTIC_HANDLE_OFFSET as u32
        );
    }

    #[test]
    fn fixed_width_operands_stay_inline() {
        let function = function(vec![Instruction {
            pc: 99,
            op: Op::Add,
            operands: vec![
                Operand::Register(0),
                Operand::Register(1),
                Operand::Register(2),
            ],
        }]);
        let module = module(function);

        let executable = ExecutableModule::from_bytecode(&module);
        let function = executable.function(0).unwrap();
        let instr = &function.code[0];

        assert_eq!(function.op(instr), Op::Add);
        assert!(function.operands_are_inline(instr));
        assert_eq!(std::mem::size_of::<otter_bytecode::WordInstruction>(), 24);
        assert_eq!(std::mem::size_of::<CodeBlockInstruction>(), 32);
        assert_eq!(function.register(instr, 0), Some(0));
        assert_eq!(function.register(instr, 1), Some(1));
        assert_eq!(function.register(instr, 2), Some(2));
        assert_eq!(function.register(instr, 3), None);
        assert_eq!(
            function.operands(instr).as_slice(),
            &[
                Operand::Register(0),
                Operand::Register(1),
                Operand::Register(2)
            ]
        );
    }

    #[test]
    fn schema_accessors_round_trip_full_word_payloads() {
        let mut builder = FunctionCodeBuilder::new();
        builder.push(
            Op::LoadInt32,
            &[Operand::Register(60000), Operand::Imm32(i32::MIN)],
        );
        builder.push(
            Op::LoadNumber,
            &[Operand::Register(7), Operand::ConstIndex(u32::MAX)],
        );
        let wordcode = builder.finish();
        let mut overflow = Vec::new();
        let load_int = CodeBlockInstruction::from_wordcode(
            &wordcode,
            0,
            0,
            0,
            NO_PROPERTY_IC_SITE,
            &mut overflow,
        );
        let load_number = CodeBlockInstruction::from_wordcode(
            &wordcode,
            1,
            0,
            1,
            NO_PROPERTY_IC_SITE,
            &mut overflow,
        );

        assert_eq!(load_int.reg(0), 60000);
        assert_eq!(load_int.imm(1), i32::MIN);
        assert_eq!(load_number.reg(0), 7);
        assert_eq!(load_number.const_word(1), u32::MAX);
    }

    #[test]
    fn long_variadic_operands_use_codeblock_side_table() {
        let operands = vec![
            Operand::Register(0),
            Operand::Register(1),
            Operand::ConstIndex(2),
            Operand::Register(2),
            Operand::Register(3),
        ];
        let function = function(vec![Instruction {
            pc: 7,
            op: Op::Call,
            operands: operands.clone(),
        }]);
        let module = module(function);

        let executable = ExecutableModule::from_bytecode(&module);
        let function = executable.function(0).unwrap();
        let instr = &function.code[0];

        assert_eq!(function.op(instr), Op::Call);
        assert!(!function.operands_are_inline(instr));
        assert_eq!(function.register(instr, 0), Some(0));
        assert_eq!(function.register(instr, 1), Some(1));
        assert_eq!(function.const_index(instr, 2), Some(2));
        assert_eq!(function.register(instr, 3), Some(2));
        assert_eq!(function.register(instr, 4), Some(3));
        assert_eq!(function.register(instr, 5), None);
        assert_eq!(function.operands(instr).as_slice(), operands.as_slice());
    }

    #[test]
    fn long_fixed_operands_use_codeblock_overflow_table() {
        let operands = vec![
            Operand::Register(0),
            Operand::Register(1),
            Operand::Register(2),
            Operand::Register(3),
            Operand::Register(4),
        ];
        let function = function(vec![Instruction {
            pc: 0,
            op: Op::MakeClass,
            operands: operands.clone(),
        }]);
        let executable = ExecutableModule::from_bytecode(&module(function));
        let function = executable.function(0).unwrap();
        let instr = &function.code[0];

        assert!(!function.operands_are_inline(instr));
        assert_eq!(function.operands(instr).as_slice(), operands.as_slice());
    }

    #[test]
    fn only_cache_owning_property_ops_get_dense_ic_sites() {
        let function = function(vec![
            Instruction {
                pc: 0,
                op: Op::LoadProperty,
                operands: vec![
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::ConstIndex(0),
                ],
            },
            Instruction {
                pc: 1,
                op: Op::StoreProperty,
                operands: vec![
                    Operand::Register(1),
                    Operand::ConstIndex(0),
                    Operand::Register(0),
                    Operand::Register(2),
                ],
            },
            Instruction {
                pc: 2,
                op: Op::HasProperty,
                operands: vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(1),
                ],
            },
        ]);
        let module = module(function);

        let executable = ExecutableModule::from_bytecode(&module);
        let function = executable.function(0).unwrap();

        assert_eq!(executable.property_ic_site_end(), 2);
        assert_eq!(function.code[0].property_ic_site(), Some(0));
        assert_eq!(function.code[1].property_ic_site(), Some(1));
        assert_eq!(function.code[2].property_ic_site(), None);
    }

    #[test]
    fn jit_snapshot_reuses_codeblock_instruction_metadata() {
        let function = function(vec![
            Instruction {
                pc: 0,
                op: Op::LoadProperty,
                operands: vec![
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::ConstIndex(7),
                ],
            },
            Instruction {
                pc: 1,
                op: Op::Add,
                operands: vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(3),
                ],
            },
        ]);
        let module = module(function);

        let executable = ExecutableModule::from_bytecode(&module);
        let view = executable.functions[0].jit_compile_snapshot();

        assert_eq!(view.code_block.id, 0);
        assert!(Arc::ptr_eq(&view.code_block, &executable.functions[0]));
        assert_eq!(view.instructions.len(), 3);
        assert_eq!(view.instructions[0].op(&view.code_block), Op::LoadProperty);
        assert_eq!(view.instructions[0].byte_pc, 0);
        assert_eq!(
            view.instructions[0].property_ic_site(&view.code_block),
            Some(0)
        );
        assert_eq!(
            view.code_block
                .operands(view.instructions[0].resolve(&view.code_block))
                .as_slice(),
            &[
                Operand::Register(0),
                Operand::Register(1),
                Operand::ConstIndex(7),
            ]
        );
        assert_eq!(view.instructions[1].op(&view.code_block), Op::Add);
        assert_eq!(
            view.instructions[1].property_ic_site(&view.code_block),
            None
        );
        assert!(std::ptr::eq::<CodeBlockInstruction>(
            view.instructions[0].resolve(&view.code_block),
            &executable.functions[0].code[0]
        ));
    }

    #[test]
    fn builder_assigns_ic_sites_and_carries_variadic_operands() {
        let function = function(vec![
            Instruction {
                pc: 0,
                op: Op::LoadProperty,
                operands: vec![
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::ConstIndex(0),
                ],
            },
            Instruction {
                pc: 1,
                op: Op::Call,
                operands: vec![
                    Operand::Register(2),
                    Operand::Register(3),
                    Operand::ConstIndex(1),
                    Operand::Register(5),
                ],
            },
        ]);
        let module = module(function);

        let builder = ExecutableModuleBuilder::from_bytecode(&module);
        assert_eq!(builder.functions.len(), 1);
        assert_eq!(builder.next_property_ic_site, 1);

        let executable = builder.freeze();
        let exec_fn = executable.function(0).unwrap();
        assert_eq!(exec_fn.code.len(), 3);
        assert_eq!(executable.property_ic_site_end(), 1);
        assert_eq!(
            exec_fn.operands(&exec_fn.code[1]).as_slice(),
            &[
                Operand::Register(2),
                Operand::Register(3),
                Operand::ConstIndex(1),
                Operand::Register(5)
            ]
        );
    }

    #[test]
    #[should_panic(expected = "invalid Call wordcode operands")]
    fn wordcode_builder_rejects_unverified_variadic_layout() {
        let function = function(vec![Instruction {
            pc: 0,
            op: Op::Call,
            operands: vec![
                Operand::Register(0),
                Operand::Register(1),
                Operand::ConstIndex(2),
                Operand::Register(2),
            ],
        }]);

        let _ = ExecutableModule::from_bytecode(&module(function));
    }
}
