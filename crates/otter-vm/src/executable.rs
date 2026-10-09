//! Authoritative immutable CodeBlock execution representation.
//!
//! `otter-bytecode` owns the compiler/debug DTO shape. The VM owns this
//! compact view so hot dispatch reads opcodes, verified operand words, and
//! named-property IC sites from one record while byte coordinates stay cold.
//!
//! # Contents
//! - [`ExecutableModule`] — VM-owned function table whose CodeBlocks are
//!   verified and built on first use.
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
//!   the next instruction.
//! - Named property IC sites receive dense VM-local ids counted per function
//!   at link time, so a function built later gets the same ids; the bytecode
//!   JSON dump stays unchanged.
//! - A function's CodeBlock exists only after the function was asked for; it
//!   is built from its admitted bytecode once its own proof is established.
//!   Passes over a module's code (IC sweeps, statistics) see built blocks only.
//! - A tier-neutral [`crate::feedback::FeedbackVector`] owns both dense
//!   instruction cells and their monotonic material-transition epoch.
//! - Execution feedback belongs to one isolate. Snapshot copies preserve
//!   admitted code while creating empty feedback through `executable_snapshot`.
//! - Each escapable CodeBlock retains its own physical-body lease. The function
//!   pointer table and independently escapable source-work cell own separate
//!   leases; no snapshot/JIT handle depends on a donor chunk's aggregate charge.
//!
//! # See also
//! - [`crate::execution_context`]
//! - [`otter_bytecode::Instruction`]

#[path = "executable_allocation.rs"]
pub(crate) mod allocation;
#[path = "code_block_cfg.rs"]
pub(crate) mod code_block_cfg;
#[path = "executable_snapshot.rs"]
mod executable_snapshot;

use otter_bytecode::{
    ArgumentBindingStorage, ArgumentsObjectKind, Function, FunctionCode, FunctionCodeBuilder, Op,
    Operand, SpanEntry, VerifiedFunction, encoding::measure_wordcode_function,
};
use otter_resource::{ResourceAccount, ResourceClass, ResourceError, ResourceLease};
use std::sync::{Arc, OnceLock};

use code_block_cfg::CodeBlockControlFlow;

pub(crate) const NO_PROPERTY_IC_SITE: u32 = u32::MAX;

/// VM-owned executable view of a bytecode module: one [`CodeBlock`] per
/// function, verified and built the first time the function is asked for.
#[derive(Debug)]
pub(crate) struct ExecutableModule {
    functions: Box<[OnceLock<Arc<CodeBlock>>]>,
    bytecode: Arc<crate::code_space::LinkedBytecode>,
    /// First dense property-IC site id of each function.
    ic_bases: Box<[u32]>,
    property_ic_site_end: u32,
    /// The account every built block's lease is charged to.
    account: ResourceAccount,
    _table_lease: ResourceLease,
}

/// Directory entry naming a globally dense method-call site.
#[derive(Debug, Clone)]
pub(crate) struct FeedbackSlotAddress;

impl FeedbackSlotAddress {
    #[must_use]
    pub(crate) fn is_method(&self) -> bool {
        true
    }
}

/// Whether `op` owns a dense named-property IC site.
fn has_property_ic_site(op: Op) -> bool {
    matches!(
        op,
        Op::LoadProperty
            | Op::HasNamedProperty
            | Op::StoreProperty
            | Op::StorePropertyStrict
            | Op::CallMethodValue
    )
}

impl ExecutableModule {
    /// Build a lazy execution view from the compiler/debug module DTO.
    #[cfg(test)]
    #[must_use]
    pub(crate) fn from_bytecode(module: &otter_bytecode::BytecodeModule) -> Self {
        let verified = otter_bytecode::VerifiedBytecodeModule::new(module.clone())
            .expect("executable test fixture must be valid bytecode");
        let account = ResourceAccount::default();
        let linked = crate::code_space::LinkedBytecode::new(verified, &account)
            .expect("admit executable fixture bytecode");
        Self::lazy(linked, 0, &account).expect("admit executable fixture table")
    }

    /// The execution view of an admitted module whose dense property-IC
    /// site ids start at `property_ic_base`. Every function's sites are
    /// counted now, so ids stay stable however the functions are built.
    pub(crate) fn lazy(
        bytecode: Arc<crate::code_space::LinkedBytecode>,
        property_ic_base: u32,
        account: &ResourceAccount,
    ) -> Result<Self, ResourceError> {
        let count = bytecode.functions.len();
        let table_lease = account.reserve_exact(
            ResourceClass::SourceModuleBytes,
            (std::mem::size_of::<ExecutableModule>() as u64)
                .saturating_add(allocation::array_bytes::<OnceLock<Arc<CodeBlock>>>(count))
                .saturating_add(allocation::array_bytes::<u32>(count)),
        )?;
        let mut next = property_ic_base;
        let ic_bases = bytecode
            .functions
            .iter()
            .map(|function| {
                let base = next;
                let sites = function
                    .code
                    .iter()
                    .filter(|instruction| has_property_ic_site(instruction.op))
                    .count();
                next = next
                    .checked_add(u32::try_from(sites).expect("property IC sites exceed u32"))
                    .expect("property IC site table exceeds u32");
                base
            })
            .collect();
        Ok(Self {
            functions: (0..count).map(|_| OnceLock::new()).collect(),
            bytecode,
            ic_bases,
            property_ic_site_end: next,
            account: account.clone(),
            _table_lease: table_lease,
        })
    }

    /// Function-table lookup by chunk-local function index, building the
    /// function's CodeBlock on first use. `None` for a missing index or a
    /// function that fails verification or admission.
    #[must_use]
    pub(crate) fn function(&self, local_index: u32) -> Option<&CodeBlock> {
        self.function_slot(local_index).map(Arc::as_ref)
    }

    /// Shared immutable CodeBlock handle for native compilation.
    #[must_use]
    pub(crate) fn function_arc(&self, local_index: u32) -> Option<Arc<CodeBlock>> {
        self.function_slot(local_index).cloned()
    }

    fn function_slot(&self, local_index: u32) -> Option<&Arc<CodeBlock>> {
        let slot = self.functions.get(local_index as usize)?;
        if let Some(block) = slot.get() {
            return Some(block);
        }
        let index = local_index as usize;
        let proof = self.bytecode.verified().function(index).ok()?;
        let mut site = self.ic_bases[index];
        let block = CodeBlock::from_verified_bytecode(
            &self.bytecode.functions[index],
            proof,
            &self.bytecode.module,
            &mut site,
            &self.account,
        )
        .ok()?;
        Some(slot.get_or_init(|| Arc::new(block)))
    }

    /// The CodeBlocks built so far.
    fn built(&self) -> impl Iterator<Item = &Arc<CodeBlock>> {
        self.functions.iter().filter_map(OnceLock::get)
    }

    /// One past the highest dense named-property IC site id in this
    /// module (equals the site count when the IC base is zero).
    #[must_use]
    pub(crate) const fn property_ic_site_end(&self) -> u32 {
        self.property_ic_site_end
    }

    /// Exact physical leases retained by this view and its shared block/counter
    /// allocations. A block/counter keeps its charge after this table drops.
    #[must_use]
    pub(crate) fn retained_bytes(&self) -> u64 {
        self.built()
            .fold(self._table_lease.amount(), |total, block| {
                total
                    .saturating_add(block._lease.amount())
                    .saturating_add(block.source_work.retained_bytes())
            })
    }

    /// Directory entries for the method-call sites in this chunk, read off
    /// the bytecode so no function has to be built.
    pub(crate) fn feedback_slot_addresses(&self) -> Vec<(usize, FeedbackSlotAddress)> {
        let mut slots = Vec::new();
        for (function, &base) in self.bytecode.functions.iter().zip(&*self.ic_bases) {
            let mut site = base as usize;
            for instruction in function.code.iter() {
                if !has_property_ic_site(instruction.op) {
                    continue;
                }
                if instruction.op == Op::CallMethodValue {
                    slots.push((site, FeedbackSlotAddress));
                }
                site += 1;
            }
        }
        slots
    }

    /// Full-collector weak pass over every property IC of these functions.
    pub(crate) fn sweep_property_ics(&self, heap: &otter_gc::GcHeap) {
        for code_block in self.built() {
            code_block.feedback.sweep_property_ics(heap);
        }
    }

    pub(crate) fn property_ic_stats(&self) -> crate::property_ic::PropertyIcStats {
        let mut total = crate::property_ic::PropertyIcStats::default();
        for code_block in self.built() {
            total.add(code_block.feedback.property_stats());
        }
        total
    }

    #[cfg(test)]
    pub(crate) fn polymorphic_property_count(
        &self,
        kind: crate::property_ic::PropertyIcKind,
    ) -> usize {
        self.built()
            .map(|code_block| code_block.feedback.polymorphic_property_count(kind))
            .sum()
    }

    pub(crate) fn property_ic_snapshots(&self) -> Vec<crate::inspect::IcSiteSnapshot> {
        let mut out = Vec::new();
        for code_block in self.built() {
            for (instruction_index, instruction) in code_block.code.iter().enumerate() {
                let Some(site) = instruction.property_ic_site() else {
                    continue;
                };
                let (kind, inspect_kind) = match code_block.op(instruction) {
                    Op::LoadProperty | Op::HasNamedProperty | Op::CallMethodValue => (
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
    /// The exact shared source-work allocation retained by compiled code.
    #[must_use]
    pub fn source_work(&self) -> &Arc<crate::native_abi::SourceWork> {
        &self.source_work
    }

    /// Whether the function materializes `arguments`, so its activation
    /// observes its actual arguments.
    #[must_use]
    pub fn needs_arguments(&self) -> bool {
        self.needs_arguments
    }

    /// Heap bytes this code block retains beyond `size_of::<Self>()`: the
    /// instruction stream, overflow operand words, control-flow and span
    /// tables, feedback vector, and annotation-hint tables.
    #[must_use]
    pub(crate) fn retained_bytes(&self) -> u64 {
        (std::mem::size_of_val::<[ExecMappedArgumentBinding]>(&self.mapped_argument_bindings)
            as u64)
            .saturating_add(self.module_url.len() as u64)
            .saturating_add(
                std::mem::size_of_val::<[otter_bytecode::ScopeDescriptor]>(&self.scopes) as u64,
            )
            .saturating_add(self.scopes.iter().fold(0u64, |total, scope| {
                total.saturating_add(scope.retained_bytes())
            }))
            .saturating_add(std::mem::size_of_val::<[CodeBlockInstruction]>(&self.code) as u64)
            .saturating_add(std::mem::size_of_val::<[u32]>(&self.overflow_operand_words) as u64)
            .saturating_add(self.control_flow.retained_bytes())
            .saturating_add(self.feedback.retained_bytes())
            .saturating_add(std::mem::size_of_val::<[u32]>(&self.byte_pcs) as u64)
            .saturating_add(std::mem::size_of_val::<[SpanEntry]>(&self.byte_spans) as u64)
            .saturating_add(std::mem::size_of_val::<[u64]>(&self.number_hints) as u64)
            .saturating_add(std::mem::size_of_val::<[(u32, u32)]>(&self.class_hints) as u64)
    }

    /// Whether execution observes this activation's actual-argument window.
    /// A body spliced without that window cannot represent these operations.
    #[must_use]
    pub fn requires_argument_frame(&self) -> bool {
        self.needs_arguments
            || self.has_rest
            || self
                .code
                .iter()
                .any(|instruction| self.op(instruction) == Op::CallForwardArguments)
    }

    /// Build JIT feedback/layout metadata over this exact immutable CodeBlock.
    #[must_use]
    pub(crate) fn jit_compile_snapshot(self: &Arc<Self>) -> crate::jit::JitCompileSnapshot {
        let gc_header_bytes = otter_gc::header::HEADER_SIZE as u32;
        crate::jit::JitCompileSnapshot {
            code_block: Arc::clone(self),
            derived_constructor: self.is_derived_constructor,
            literal_allocations: crate::jit::JitLiteralAllocationPlans::default(),
            // Baked by `Interpreter::compile_jit_function`, which holds the
            // cage base and the live property-IC tables.
            cage_base: 0,
            // Baked alongside `cage_base` by `compile_jit_function`; the
            // all-zero default is never read because the emitter gates inline
            // element access on `cage_base != 0`.
            array_layout: crate::jit::JitArrayLayout::default(),
            element_accesses: rustc_hash::FxHashMap::default(),
            array_iteration: None,
            unseen_element_sites: rustc_hash::FxHashSet::default(),
            string_layout: crate::jit::JitStringLayout::default(),
            // `#[repr(C)]` constant: offset from the decompressed object
            // pointer to its shape handle for native CacheIR guards.
            object_shape_byte: otter_gc::header::HEADER_SIZE as u32
                + crate::object::OBJECT_BODY_SHAPE_OFFSET as u32,
            exotic_dictionary_layout_byte: otter_gc::header::HEADER_SIZE as u32
                + crate::object::EXOTIC_SLOTS_DICTIONARY_LAYOUT_OFFSET as u32,
            exotic_instance_root_byte: otter_gc::header::HEADER_SIZE as u32
                + crate::object::EXOTIC_SLOTS_INSTANCE_ROOT_OFFSET as u32,
            field_layout: crate::object::FieldLayout::current(),
            shape_property_count_byte: otter_gc::header::HEADER_SIZE as u32
                + crate::object::SHAPE_BODY_PROPERTY_COUNT_OFFSET as u32,
            object_exotic_handle_byte: otter_gc::header::HEADER_SIZE as u32
                + crate::object::OBJECT_BODY_EXOTIC_HANDLE_OFFSET as u32,
            gc_barrier: crate::jit::JitGcBarrierLayout {
                header_flags_byte: otter_gc::header::HEADER_FLAGS_BYTE_OFFSET as u32,
                young_flag: otter_gc::header::GENERATION_YOUNG_FLAG as u32,
                remembered_flag: otter_gc::header::REMEMBERED_FLAG as u32,
            },
            shape_prototype_byte: otter_gc::header::HEADER_SIZE as u32
                + crate::object::SHAPE_BODY_PROTOTYPE_OFFSET as u32,
            shape_inline_capacity_byte: otter_gc::header::HEADER_SIZE as u32
                + crate::object::SHAPE_BODY_INLINE_CAPACITY_OFFSET as u32,
            shape_state_byte: otter_gc::header::HEADER_SIZE as u32
                + crate::object::SHAPE_BODY_STATE_OFFSET as u32,
            closure_call_layout: crate::jit::JitClosureCallLayout {
                function_id_byte: gc_header_bytes
                    + crate::closure::CLOSURE_BODY_FUNCTION_ID_OFFSET as u32,
                flags_byte: gc_header_bytes + crate::closure::CLOSURE_BODY_CALL_FLAGS_OFFSET as u32,
                context_byte: gc_header_bytes + crate::closure::CLOSURE_BODY_CONTEXT_OFFSET as u32,
                bound_this_byte: gc_header_bytes
                    + crate::closure::CLOSURE_BODY_BOUND_THIS_OFFSET as u32,
                bound_new_target_byte: gc_header_bytes
                    + crate::closure::CLOSURE_BODY_BOUND_NEW_TARGET_OFFSET as u32,
                bound_this_flag: crate::closure::CLOSURE_CALL_FLAG_BOUND_THIS,
                bound_new_target_flag: crate::closure::CLOSURE_CALL_FLAG_BOUND_NEW_TARGET,
                runtime_setup_flags: crate::closure::CLOSURE_CALL_RUNTIME_SETUP_FLAGS,
                rare_byte: gc_header_bytes + crate::closure::CLOSURE_BODY_RARE_OFFSET as u32,
                own_props_byte: gc_header_bytes
                    + crate::closure_construct::CLOSURE_RARE_OWN_PROPS_OFFSET as u32,
                prototype_byte: gc_header_bytes
                    + crate::closure_construct::CLOSURE_RARE_PROTOTYPE_OFFSET as u32,
                constructor_layouts_byte: gc_header_bytes
                    + crate::closure_construct::CLOSURE_RARE_CONSTRUCTOR_LAYOUTS_OFFSET as u32,
                prototype_ordinary_byte: gc_header_bytes
                    + crate::closure_construct::CLOSURE_RARE_PROTOTYPE_ORDINARY_OFFSET as u32,
                instanceof_cached_byte: gc_header_bytes
                    + crate::closure_construct::CLOSURE_RARE_INSTANCEOF_CACHED_OFFSET as u32,
            },
            class_constructor_layout: crate::jit::JitClassConstructorLayout {
                type_tag: crate::class_constructor::CLASS_CONSTRUCTOR_BODY_TYPE_TAG,
                constructor_layouts_byte: gc_header_bytes
                    + crate::class_constructor::CLASS_CONSTRUCTOR_BODY_LAYOUTS_OFFSET as u32,
            },
            constructor_layout: crate::jit::JitConstructorLayout {
                family_id_byte: gc_header_bytes
                    + crate::constructor_layout::CONSTRUCTOR_LAYOUT_FAMILY_ID_OFFSET as u32,
                root_byte: gc_header_bytes
                    + crate::constructor_layout::CONSTRUCTOR_LAYOUT_ROOT_OFFSET as u32,
            },
            primitive_cell_type_tags: [
                crate::string::JS_STRING_BODY_TYPE_TAG,
                crate::symbol::SYMBOL_BODY_TYPE_TAG,
                crate::bigint::BIG_INT_BODY_TYPE_TAG,
            ],
            global_lexical_value_byte: otter_gc::header::HEADER_SIZE as u32
                + std::mem::offset_of!(crate::upvalue::UpvalueCellBody, value) as u32,
            context_layout: crate::jit::JitContextLayout::current(),
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
            native_call_layout: crate::jit::JitNativeCallLayout::current(),
            instructions: self
                .code
                .iter()
                .enumerate()
                .map(|(index, _)| crate::jit::JitInstructionMetadata {
                    instruction_index: index as u32,
                    byte_pc: self.byte_pcs[index],
                    // Resolved by `ExecutionContext::jit_compile_snapshot`, which
                    // can map a `MakeFunction` constant index to its target id.
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
                    property_attempted: match self.op_at(index) {
                        Some(Op::LoadProperty | Op::HasNamedProperty | Op::CallMethodValue) => self
                            .property_feedback_at(index, crate::property_ic::PropertyIcKind::Load),
                        Some(Op::StoreProperty | Op::StorePropertyStrict) => self
                            .property_feedback_at(index, crate::property_ic::PropertyIcKind::Store),
                        _ => None,
                    }
                    .is_some_and(crate::feedback::PropertyFeedbackSlot::attempted),
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
                    arith_cell: self
                        .feedback_at(index)
                        .map_or(0, crate::feedback::InstructionFeedback::arith_cell_address),
                })
                .collect(),
            // Baked by `Interpreter::bake_global_lexical_loads`, which owns the
            // live global declarative record. Raw snapshots carry no GC cell
            // identity.
            global_lexical_loads: rustc_hash::FxHashMap::default(),
            // Baked by `Interpreter::bake_literal_cells`, which owns
            // the address-stable traced literal cells. Raw snapshots carry no
            // process-local cell identity.
            literal_cells: rustc_hash::FxHashMap::default(),
            // Baked by `Interpreter::bake_global_lexical_loads`, which owns
            // the live global declarative record and global object.
            global_object_loads: rustc_hash::FxHashMap::default(),
            // Baked from the authoritative typed `Op::Call` distribution by
            // `Interpreter::bake_inline_callees`.
            native_calls: rustc_hash::FxHashMap::default(),
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
            function_prototype_calls: rustc_hash::FxHashMap::default(),
            instanceof_cells: rustc_hash::FxHashMap::default(),
            array_constructor_sites: rustc_hash::FxHashMap::default(),
            forward_apply_native_ref: None,
            property_programs: rustc_hash::FxHashMap::default(),
            property_action_cache: None,
            property_accesses: rustc_hash::FxHashMap::default(),
            binding_hit_proofs: rustc_hash::FxHashMap::default(),
            context_allocations: rustc_hash::FxHashMap::default(),
            closure_allocations: rustc_hash::FxHashMap::default(),
            optimized_exit_reasons: std::collections::BTreeMap::new(),
            feedback_exits: std::collections::BTreeSet::new(),
            parameter_widening: Box::default(),
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
        handlers: &[otter_bytecode::ExceptionHandler],
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
        let account = ResourceAccount::default();
        let mut fixture_lease = account
            .reserve_exact(
                ResourceClass::SourceModuleBytes,
                crate::feedback::FeedbackVector::allocation_bytes(
                    instructions.iter().map(|instruction| instruction.op),
                )
                .saturating_add(CodeBlockControlFlow::allocation_bytes(&wordcode, handlers)),
            )
            .expect("admit fixture feedback/control-flow");
        let feedback = crate::feedback::FeedbackVector::for_instruction_ops(
            instructions.iter().map(|instruction| instruction.op),
            &mut fixture_lease,
        )
        .expect("prepare fixture feedback");
        let control_flow =
            CodeBlockControlFlow::from_verified_wordcode(&wordcode, handlers, &mut fixture_lease)
                .expect("prepare fixture control flow");
        let mut body = Self {
            _lease: account
                .reserve_exact(
                    ResourceClass::SourceModuleBytes,
                    std::mem::size_of::<Self>() as u64,
                )
                .expect("admit fixture body"),
            id,
            param_count,
            register_count,
            is_strict: false,
            is_arrow: false,
            asm_module: false,
            is_method: false,
            has_rest: false,
            is_async: false,
            is_generator: false,
            is_async_generator: false,
            is_derived_constructor: false,
            makes_function: false,
            observes_this: true,
            needs_arguments: false,
            arguments_object_kind: ArgumentsObjectKind::Unmapped,
            mapped_argument_bindings: Box::new([]),
            is_module: false,
            module_url: Box::<str>::from(""),
            scopes: Box::new([]),
            contains_direct_eval: false,
            primordial_iteration: false,
            code: code.into_boxed_slice(),
            overflow_operand_words: overflow_operand_words.into_boxed_slice(),
            bytecode_byte_len,
            control_flow,
            feedback,
            source_work: Arc::new(
                crate::native_abi::SourceWork::new(&account).expect("admit fixture work cell"),
            ),
            byte_pcs: byte_pcs.into_boxed_slice(),
            byte_spans: Box::new([]),
            number_hints: Box::new([]),
            class_hints: Box::new([]),
        };
        body._lease
            .resize((std::mem::size_of::<Self>() as u64).saturating_add(body.retained_bytes()))
            .expect("admit fixture body tables");
        Arc::new(body)
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

    /// Receiver programs the inline cache of the named property site at
    /// instruction `index` holds ([`crate::feedback::PropertyFeedbackSlot::population`]);
    /// `None` when the instruction is not a named property access.
    #[must_use]
    pub(crate) fn property_site_population(&self, index: usize) -> Option<u32> {
        let kind = match self.op_at(index)? {
            Op::LoadProperty | Op::HasNamedProperty | Op::CallMethodValue => {
                crate::property_ic::PropertyIcKind::Load
            }
            Op::StoreProperty | Op::StorePropertyStrict => {
                crate::property_ic::PropertyIcKind::Store
            }
            _ => return None,
        };
        Some(self.property_feedback_at(index, kind)?.population())
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

    /// Last finalized actual constructor family selected at this exact source
    /// site. No GC value or template-id alias is recorded here.
    pub(crate) fn construct_family_at(&self, index: usize) -> Option<u64> {
        self.feedback.call_slot(index)?.construct_family()
    }

    /// Record a monotonic scalar family through the same dense owner. The
    /// noalloc observation is evaluated only while this exact cell can change.
    pub(crate) fn record_construct_family(&self, index: usize, observe: impl FnOnce() -> u64) {
        self.feedback.record_construct_family(index, observe);
    }

    /// Cold serialized byte PC for one logical instruction index.
    #[must_use]
    pub(crate) fn instruction_byte_pc(&self, index: usize) -> Option<u32> {
        self.byte_pcs.get(index).copied()
    }

    /// The logical instruction index whose serialized byte PC is `byte_pc`.
    /// Byte PCs ascend with the index.
    #[must_use]
    pub(crate) fn instruction_at_byte_pc(&self, byte_pc: u32) -> Option<usize> {
        self.byte_pcs.binary_search(&byte_pc).ok()
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

    /// `true` when this function is an arrow function.
    #[must_use]
    pub const fn is_arrow(&self) -> bool {
        self.is_arrow
    }

    /// Whether an activation's `this` binding is observable: the body reads
    /// it, creates a closure or holds a direct eval.
    #[must_use]
    pub const fn observes_this(&self) -> bool {
        self.observes_this
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

    /// Whether an optimizing caller may build this body in place of a call:
    /// without handlers and at most
    /// [`crate::jit::JIT_MAX_INLINED_BYTECODE_BYTES`] long. Its callers
    /// prepare inline snapshots only for such bodies.
    #[must_use]
    pub fn admits_graph_inlining(&self) -> bool {
        self.bytecode_byte_len <= crate::jit::JIT_MAX_INLINED_BYTECODE_BYTES
            && self.control_flow().handlers().is_empty()
    }

    /// Whether legacy `fn.arguments` reads an activation of this body: an
    /// ordinary sloppy function (not arrow, method, generator or async), as
    /// `Interpreter::legacy_function_metadata_eligible` admits. An inlined
    /// frame of such a body records its actual arguments for deopt.
    #[must_use]
    pub fn exposes_legacy_arguments(&self) -> bool {
        !self.is_strict
            && !self.is_arrow
            && !self.is_method
            && !self.is_generator
            && !self.is_async
            && !self.is_async_generator
    }

    /// Last logical backedge PC for a loop header.
    #[must_use]
    pub(crate) fn loop_latch(&self, header_pc: u32) -> Option<u32> {
        self.control_flow.loop_latch(header_pc)
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
    /// `true` when this function uses strict-mode call semantics.
    pub is_strict: bool,
    /// `true` when this function is an arrow function.
    pub(crate) is_arrow: bool,
    /// `true` when the body declares `"use asm"`: its first interpreted entry
    /// offers it to the host asm.js linker.
    pub(crate) asm_module: bool,
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
    /// `Op::MakeClosure`.
    pub(crate) makes_function: bool,
    /// Whether an activation's `this` binding is observable: the bytecode
    /// does not declare it ignored ([`Function::ignores_this`], checked
    /// against the body). Callers skip receiver conversion otherwise.
    pub(crate) observes_this: bool,
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
    /// Scope descriptors of every context this function creates, indexed by
    /// `Op::CreateContext`'s scope operand. A live context names one entry by
    /// `(id, scope index)`; run-time TDZ messages and direct eval read slot
    /// names and kinds from here.
    pub(crate) scopes: Box<[otter_bytecode::ScopeDescriptor]>,
    /// `true` when this function's own code contains a direct eval call
    /// site, so its activation stays materialized.
    pub(crate) contains_direct_eval: bool,
    /// Runtime-internal code iterating with intrinsic algorithms
    /// (see [`otter_bytecode::Function::primordial_iteration`]).
    pub(crate) primordial_iteration: bool,
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
    /// Address-stable entered-opcode attempts; policy and native code retain
    /// this same allocation, never a copied execution ledger.
    source_work: Arc<crate::native_abi::SourceWork>,
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
    _lease: ResourceLease,
}

/// `FUNCTION_CALL_*` bits of an admitted bytecode function: exactly the
/// bits [`CodeBlock::call_flags`] reports once its body is built, so module
/// linking publishes them without building it.
pub(crate) fn bytecode_call_flags(function: &Function) -> u32 {
    call_flags(
        function.is_strict,
        function.is_arrow,
        function.is_method,
        !function.ignores_this,
        function.is_derived_constructor,
        function.is_async || function.is_generator || function.is_async_generator,
    )
}

const fn call_flags(
    is_strict: bool,
    is_arrow: bool,
    is_method: bool,
    observes_this: bool,
    is_derived_constructor: bool,
    suspendable: bool,
) -> u32 {
    use crate::native_abi::{
        FUNCTION_CALL_CONSTRUCTIBLE, FUNCTION_CALL_DERIVED_CONSTRUCTOR, FUNCTION_CALL_LEXICAL_THIS,
        FUNCTION_CALL_NO_RECEIVER_CONVERSION, FUNCTION_CALL_SUSPENDABLE,
    };
    let mut flags = 0;
    if is_strict || is_arrow || !observes_this {
        flags |= FUNCTION_CALL_NO_RECEIVER_CONVERSION;
    }
    if is_derived_constructor {
        flags |= FUNCTION_CALL_DERIVED_CONSTRUCTOR;
    }
    if !is_arrow && !is_method && !suspendable {
        flags |= FUNCTION_CALL_CONSTRUCTIBLE;
    }
    if suspendable {
        flags |= FUNCTION_CALL_SUSPENDABLE;
    }
    if is_arrow {
        flags |= FUNCTION_CALL_LEXICAL_THIS;
    }
    flags
}

impl CodeBlock {
    /// Immutable call flags of this function, the `FUNCTION_CALL_*` bits
    /// its [`crate::native_abi::FunctionEntryCell`] publishes: receiver
    /// conversion, derived constructor, `[[Construct]]`, suspendable body and
    /// lexical `this`.
    /// The call trampoline and every generated call entry bind by these bits.
    #[must_use]
    pub fn call_flags(&self) -> u32 {
        call_flags(
            self.is_strict,
            self.is_arrow,
            self.is_method,
            self.observes_this,
            self.is_derived_constructor,
            self.is_async || self.is_generator || self.is_async_generator,
        )
    }

    fn from_verified_bytecode(
        function: &Function,
        proof: &VerifiedFunction,
        module_url: &str,
        next_property_ic_site: &mut u32,
        account: &ResourceAccount,
    ) -> Result<Self, ResourceError> {
        let register_count = proof.register_count();
        let code_byte_len = proof.layout().total_bytes;
        let count = function.code.len();
        let overflow_count = function
            .code
            .iter()
            .map(|instruction| {
                if instruction.operand_count() > 4 {
                    instruction.operand_count()
                } else {
                    0
                }
            })
            .sum::<usize>();
        let module_url = if function.module_url.is_empty() {
            module_url
        } else {
            &function.module_url
        };
        let mut requested = (std::mem::size_of::<Self>() as u64)
            .saturating_add(allocation::array_bytes::<CodeBlockInstruction>(count))
            .saturating_add(allocation::array_bytes::<u32>(overflow_count))
            .saturating_add(allocation::array_bytes::<u32>(count))
            .saturating_add(allocation::array_bytes::<SpanEntry>(function.spans.len()))
            .saturating_add(allocation::array_bytes::<ExecMappedArgumentBinding>(
                function.mapped_argument_bindings.len(),
            ))
            .saturating_add(allocation::array_bytes::<otter_bytecode::ScopeDescriptor>(
                function.scopes.len(),
            ))
            .saturating_add(module_url.len() as u64)
            .saturating_add(crate::feedback::FeedbackVector::allocation_bytes(
                function.code.iter().map(|instruction| instruction.op),
            ))
            .saturating_add(CodeBlockControlFlow::allocation_bytes(
                &function.code,
                &function.handlers,
            ))
            .saturating_add(allocation::array_bytes::<(u32, u32)>(
                function.class_hint_sites.len(),
            ));
        if !function.number_hint_sites.is_empty() {
            requested =
                requested.saturating_add(allocation::array_bytes::<u64>(count.div_ceil(64)));
        }
        for scope in &function.scopes {
            requested = requested.saturating_add(allocation::array_bytes::<
                otter_bytecode::SlotDescriptor,
            >(scope.slots.len()));
            for slot in &scope.slots {
                requested = requested.saturating_add(slot.name.len() as u64);
            }
        }
        let mut lease = account.reserve_exact(ResourceClass::SourceModuleBytes, requested)?;
        let instr_to_byte_pc = allocation::try_copy(&proof.layout().instr_to_byte_pc, &mut lease)?;
        let control_flow = CodeBlockControlFlow::from_verified_wordcode(
            &function.code,
            &function.handlers,
            &mut lease,
        )?;
        let mut overflow_operand_words = allocation::try_vec(overflow_count, &mut lease)?;
        let mut code = allocation::try_vec(count, &mut lease)?;
        for (idx, instruction) in function.code.iter().enumerate() {
            let property_ic_site = if has_property_ic_site(instruction.op) {
                let site = *next_property_ic_site;
                *next_property_ic_site = next_property_ic_site
                    .checked_add(1)
                    .expect("property IC site table exceeds u32");
                site
            } else {
                NO_PROPERTY_IC_SITE
            };
            code.push(CodeBlockInstruction::from_wordcode(
                &function.code,
                idx,
                function.id,
                idx as u32,
                property_ic_site,
                &mut overflow_operand_words,
            ));
        }
        let code = code.into_boxed_slice();
        let feedback = crate::feedback::FeedbackVector::for_instruction_ops(
            function.code.iter().map(|instruction| instruction.op),
            &mut lease,
        )?;
        let number_hints = if function.number_hint_sites.is_empty() {
            Box::new([]) as Box<[u64]>
        } else {
            let mut bits = allocation::try_vec(count.div_ceil(64), &mut lease)?;
            bits.resize(count.div_ceil(64), 0u64);
            for &site in &function.number_hint_sites {
                let index = site as usize;
                if index < code.len() {
                    bits[index / 64] |= 1 << (index % 64);
                }
            }
            bits.into_boxed_slice()
        };
        let mut class_hints = allocation::try_vec(function.class_hint_sites.len(), &mut lease)?;
        class_hints.extend(
            function
                .class_hint_sites
                .iter()
                .filter(|site| (site.pc as usize) < code.len())
                .map(|site| (site.pc, site.class_function_id)),
        );
        class_hints.sort_unstable_by_key(|&(pc, _)| pc);
        class_hints.dedup_by_key(|&mut (pc, _)| pc);
        let class_hints = class_hints.into_boxed_slice();
        let mut mapped_argument_bindings =
            allocation::try_vec(function.mapped_argument_bindings.len(), &mut lease)?;
        mapped_argument_bindings.extend(function.mapped_argument_bindings.iter().map(|binding| {
            ExecMappedArgumentBinding {
                argument_index: binding.argument_index,
                storage: binding.storage,
            }
        }));
        let mapped_argument_bindings = mapped_argument_bindings.into_boxed_slice();
        let mut byte_spans = allocation::try_vec(function.spans.len(), &mut lease)?;
        byte_spans.extend(function.spans.iter().map(|entry| {
            SpanEntry {
                pc: instr_to_byte_pc
                    .get(entry.pc as usize)
                    .copied()
                    .unwrap_or(code_byte_len),
                span: entry.span,
            }
        }));
        let byte_spans = byte_spans.into_boxed_slice();
        let module_url = allocation::try_string(module_url, &mut lease)?.into_boxed_str();
        let scopes = allocation::try_scopes(&function.scopes, &mut lease)?;
        let makes_function = function
            .code
            .iter()
            .any(|instr| matches!(instr.op, Op::MakeFunction | Op::MakeClosure));
        let observes_this = !function.ignores_this;
        let mut body = Self {
            _lease: lease,
            id: function.id,
            makes_function,
            observes_this,
            param_count: function.param_count,
            register_count,
            is_strict: function.is_strict,
            is_arrow: function.is_arrow,
            asm_module: function.asm_module,
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
            module_url,
            scopes,
            contains_direct_eval: function.contains_direct_eval,
            primordial_iteration: function.primordial_iteration,
            code,
            overflow_operand_words: overflow_operand_words.into_boxed_slice(),
            bytecode_byte_len: code_byte_len,
            control_flow,
            feedback,
            source_work: Arc::new(crate::native_abi::SourceWork::new(account)?),
            byte_pcs: instr_to_byte_pc,
            byte_spans,
            number_hints,
            class_hints,
        };
        body._lease
            .resize((std::mem::size_of::<Self>() as u64).saturating_add(body.retained_bytes()))?;
        Ok(body)
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
                let access =
                    otter_bytecode::opcode_schema::register_access_at(instruction.op, index);
                if access == otter_bytecode::opcode_schema::RegisterAccess::None {
                    continue;
                }
                let register = match instruction.operands[index] {
                    Operand::Register(value) => u32::from(value),
                    Operand::Imm32(value) => u32::try_from(value).unwrap_or(0),
                    Operand::ConstIndex(value) => value,
                };
                max_register = max_register.max(register + u32::from(access.width()));
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
                context_byte: gc_header_bytes + crate::closure::CLOSURE_BODY_CONTEXT_OFFSET as u32,
                bound_this_byte: gc_header_bytes
                    + crate::closure::CLOSURE_BODY_BOUND_THIS_OFFSET as u32,
                bound_new_target_byte: gc_header_bytes
                    + crate::closure::CLOSURE_BODY_BOUND_NEW_TARGET_OFFSET as u32,
                bound_this_flag: crate::closure::CLOSURE_CALL_FLAG_BOUND_THIS,
                bound_new_target_flag: crate::closure::CLOSURE_CALL_FLAG_BOUND_NEW_TARGET,
                runtime_setup_flags: crate::closure::CLOSURE_CALL_RUNTIME_SETUP_FLAGS,
                rare_byte: gc_header_bytes + crate::closure::CLOSURE_BODY_RARE_OFFSET as u32,
                own_props_byte: gc_header_bytes
                    + crate::closure_construct::CLOSURE_RARE_OWN_PROPS_OFFSET as u32,
                prototype_byte: gc_header_bytes
                    + crate::closure_construct::CLOSURE_RARE_PROTOTYPE_OFFSET as u32,
                constructor_layouts_byte: gc_header_bytes
                    + crate::closure_construct::CLOSURE_RARE_CONSTRUCTOR_LAYOUTS_OFFSET as u32,
                prototype_ordinary_byte: gc_header_bytes
                    + crate::closure_construct::CLOSURE_RARE_PROTOTYPE_ORDINARY_OFFSET as u32,
                instanceof_cached_byte: gc_header_bytes
                    + crate::closure_construct::CLOSURE_RARE_INSTANCEOF_CACHED_OFFSET as u32,
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
            snapshot.shape_state_byte,
            header + crate::object::SHAPE_BODY_STATE_OFFSET as u32
        );
        assert_eq!(
            snapshot.shape_inline_capacity_byte,
            otter_gc::header::HEADER_SIZE as u32
                + crate::object::SHAPE_BODY_INLINE_CAPACITY_OFFSET as u32
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
        let block = executable.function_arc(0).unwrap();
        let view = block.jit_compile_snapshot();

        assert_eq!(view.code_block.id, 0);
        assert!(Arc::ptr_eq(&view.code_block, &block));
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
            &block.code[0]
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

        let executable = ExecutableModule::from_bytecode(&module);
        assert_eq!(executable.functions.len(), 1);
        assert!(
            executable.functions[0].get().is_none(),
            "built on first use"
        );
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
