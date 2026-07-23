//! Authoritative immutable CodeBlock execution representation.
//!
//! `otter-bytecode` owns the compiler/debug DTO shape. The VM owns this
//! compact view so hot dispatch reads opcodes, verified operand words, and
//! named-property IC sites from one record while byte coordinates stay cold.
//!
//! # Contents
//! - [`ExecutableModuleBuilder`] — transient builder over compiler bytecode.
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
//! - Operand payloads are untagged 32-bit words verified against the opcode
//!   schema while the CodeBlock is built. Typed hot accessors read them without
//!   repeating schema or function-table lookup.
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
    ArgumentBindingStorage, ArgumentsObjectKind, BytecodeModule, Function, FunctionCode,
    FunctionCodeBuilder, Op, Operand, SpanEntry,
    encoding::{
        FunctionLayout, layout_wordcode_function, measure_wordcode_function,
        translate_spans_to_byte_pcs,
    },
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
    pub(crate) fn from_bytecode(module: &BytecodeModule) -> Self {
        Self::from_bytecode_with_ic_base(module, 0)
    }

    /// Build a transient executable view whose dense property-IC site
    /// ids start at `property_ic_base`, keeping sites globally unique
    /// across chunks linked into one interpreter.
    #[must_use]
    pub(crate) fn from_bytecode_with_ic_base(
        module: &BytecodeModule,
        property_ic_base: u32,
    ) -> Self {
        let mut builder = Self {
            functions: Vec::with_capacity(module.functions.len()),
            next_property_ic_site: property_ic_base,
        };
        for function in &module.functions {
            builder.push_function(function, &module.module);
        }
        builder
    }

    fn push_function(&mut self, function: &Function, module_url: &str) {
        let function = Arc::new(CodeBlock::from_bytecode(
            function,
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

/// Stable directory entry mapping a globally dense property/method site id
/// back to its owning CodeBlock feedback slot or method marker.
#[derive(Debug, Clone)]
pub(crate) struct FeedbackSlotAddress {
    code_block: Arc<CodeBlock>,
    instruction_index: usize,
}

impl FeedbackSlotAddress {
    #[must_use]
    pub(crate) fn property(
        &self,
        kind: crate::property_ic::PropertyIcKind,
    ) -> Option<crate::feedback::PropertyFeedbackSlot<'_>> {
        self.code_block
            .feedback
            .property_slot(self.instruction_index, kind)
    }

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
    pub(crate) fn from_bytecode(module: &BytecodeModule) -> Self {
        ExecutableModuleBuilder::from_bytecode(module).freeze()
    }

    /// Build a frozen execution view whose dense property-IC site ids
    /// start at `property_ic_base`.
    #[must_use]
    pub(crate) fn from_bytecode_with_ic_base(
        module: &BytecodeModule,
        property_ic_base: u32,
    ) -> Self {
        ExecutableModuleBuilder::from_bytecode_with_ic_base(module, property_ic_base).freeze()
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

    /// Build directory entries for the property/method sites in this chunk.
    pub(crate) fn feedback_slot_addresses(&self) -> Vec<(usize, FeedbackSlotAddress)> {
        let mut slots = Vec::new();
        for code_block in &self.functions {
            for (instruction_index, instruction) in code_block.code.iter().enumerate() {
                if let Some(site) = instruction.property_ic_site() {
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
}

impl CodeBlock {
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
            string_layout: crate::jit::JitStringLayout::default(),
            // `#[repr(C)]` constant: offset from the decompressed object
            // pointer to its shape handle, for the WhiskerIC load-cell guard.
            object_shape_byte: otter_gc::header::HEADER_SIZE as u32
                + crate::object::OBJECT_BODY_SHAPE_OFFSET as u32,
            object_dictionary_shape_id_byte: otter_gc::header::HEADER_SIZE as u32
                + crate::object::OBJECT_BODY_DICTIONARY_SHAPE_ID_OFFSET as u32,
            object_values_ptr_byte: otter_gc::header::HEADER_SIZE as u32
                + crate::object::OBJECT_BODY_VALUES_PTR_OFFSET as u32,
            object_inline_values_byte: otter_gc::header::HEADER_SIZE as u32
                + crate::object::OBJECT_BODY_INLINE_VALUES_OFFSET as u32,
            object_slab_len_byte: otter_gc::header::HEADER_SIZE as u32
                + crate::object::OBJECT_BODY_SLAB_LEN_OFFSET as u32,
            object_inline_slot_cap: crate::object::INLINE_SLOT_CAP as u32,
            gc_barrier: crate::jit::JitGcBarrierLayout {
                header_flags_byte: otter_gc::header::HEADER_FLAGS_BYTE_OFFSET as u32,
                young_flag: otter_gc::header::GENERATION_YOUNG_FLAG as u32,
                card_bitmap_byte: std::mem::offset_of!(otter_gc::page::PageHeader, card_bitmap)
                    as u32,
                page_mask: !(otter_gc::PAGE_SIZE as u64 - 1),
                card_shift: otter_gc::CARD_SIZE.trailing_zeros(),
            },
            jit_proto_byte: otter_gc::header::HEADER_SIZE as u32
                + crate::object::OBJECT_BODY_JIT_PROTO_OFFSET as u32,
            heap_number_type_tag: crate::heap_number::HEAP_NUMBER_TYPE_TAG,
            heap_number_bits_byte: otter_gc::header::HEADER_SIZE as u32
                + std::mem::offset_of!(crate::heap_number::HeapNumberBody, bits) as u32,
            closure_call_layout: crate::jit::JitClosureCallLayout {
                function_id_byte: gc_header_bytes
                    + crate::closure::CLOSURE_BODY_FUNCTION_ID_OFFSET as u32,
                flags_byte: gc_header_bytes + crate::closure::CLOSURE_BODY_CALL_FLAGS_OFFSET as u32,
                upvalue_base_byte: gc_header_bytes
                    + crate::closure::CLOSURE_BODY_UPVALUE_BASE_OFFSET as u32,
                upvalue_count_byte: gc_header_bytes
                    + crate::closure::CLOSURE_BODY_UPVALUE_COUNT_OFFSET as u32,
                bound_this_byte: gc_header_bytes
                    + crate::closure::CLOSURE_BODY_BOUND_THIS_OFFSET as u32,
                bound_new_target_byte: gc_header_bytes
                    + crate::closure::CLOSURE_BODY_BOUND_NEW_TARGET_OFFSET as u32,
                bound_this_flag: crate::closure::CLOSURE_CALL_FLAG_BOUND_THIS,
                bound_new_target_flag: crate::closure::CLOSURE_CALL_FLAG_BOUND_NEW_TARGET,
                runtime_setup_flags: crate::closure::CLOSURE_CALL_RUNTIME_SETUP_FLAGS,
            },
            upvalue_value_byte: otter_gc::header::HEADER_SIZE as u32
                + std::mem::offset_of!(crate::upvalue::UpvalueCellBody, value) as u32,
            collection_layout: crate::jit::JitCollectionLayout {
                map_type_tag: crate::collections::MAP_BODY_TYPE_TAG,
                set_type_tag: crate::collections::SET_BODY_TYPE_TAG,
                guard_flags_byte: otter_gc::header::HEADER_SIZE as u32
                    + crate::collections::MAP_BODY_JIT_GUARD_FLAGS_OFFSET as u32,
                native_function_type_tag: crate::native_function::NATIVE_FUNCTION_BODY_TYPE_TAG,
            },
            native_static_fn_byte: otter_gc::header::HEADER_SIZE as u32
                + crate::native_function::NATIVE_FUNCTION_BODY_JIT_STATIC_FN_OFFSET as u32,
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
            direct_methods: rustc_hash::FxHashMap::default(),
            inline_callees: rustc_hash::FxHashMap::default(),
            inline_methods: rustc_hash::FxHashMap::default(),
            inline_poly_methods: rustc_hash::FxHashMap::default(),
            collection_leaf_methods: rustc_hash::FxHashMap::default(),
            collection_alloc_methods: rustc_hash::FxHashMap::default(),
            array_methods: rustc_hash::FxHashMap::default(),
            primitive_method_guards: rustc_hash::FxHashMap::default(),
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
            uses_arguments_callee: false,
            arguments_object_kind: ArgumentsObjectKind::Unmapped,
            mapped_argument_bindings: Box::new([]),
            is_module: false,
            module_url: Box::<str>::from(""),
            direct_eval_bindings: Box::new([]),
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
    /// Mirrors [`otter_bytecode::Function::uses_arguments_callee`].
    pub(crate) uses_arguments_callee: bool,
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

impl Clone for CodeBlock {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            param_count: self.param_count,
            register_count: self.register_count,
            own_upvalue_count: self.own_upvalue_count,
            is_strict: self.is_strict,
            is_arrow: self.is_arrow,
            is_method: self.is_method,
            has_rest: self.has_rest,
            is_async: self.is_async,
            is_generator: self.is_generator,
            is_async_generator: self.is_async_generator,
            is_derived_constructor: self.is_derived_constructor,
            makes_function: self.makes_function,
            needs_arguments: self.needs_arguments,
            uses_arguments_callee: self.uses_arguments_callee,
            arguments_object_kind: self.arguments_object_kind,
            mapped_argument_bindings: self.mapped_argument_bindings.clone(),
            is_module: self.is_module,
            module_url: self.module_url.clone(),
            direct_eval_bindings: self.direct_eval_bindings.clone(),
            contains_direct_eval: self.contains_direct_eval,
            code: self.code.clone(),
            overflow_operand_words: self.overflow_operand_words.clone(),
            bytecode_byte_len: self.bytecode_byte_len,
            control_flow: self.control_flow.clone(),
            feedback: self.feedback.clone(),
            byte_pcs: self.byte_pcs.clone(),
            byte_spans: self.byte_spans.clone(),
            number_hints: self.number_hints.clone(),
            class_hints: self.class_hints.clone(),
        }
    }
}

impl CodeBlock {
    fn from_bytecode(
        function: &Function,
        module_url: &str,
        next_property_ic_site: &mut u32,
    ) -> Self {
        let register_count = function
            .param_count
            .saturating_add(function.locals)
            .saturating_add(function.scratch);
        let FunctionLayout {
            total_bytes: code_byte_len,
            instr_to_byte_pc,
        } = layout_wordcode_function(&function.code).unwrap_or_else(|error| {
            panic!(
                "compiler emitted bytecode that violates the opcode schema: function={} id={}: {error}",
                function.name, function.id
            )
        });
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
                    | Op::HasProperty
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
            is_strict: function.is_strict,
            is_arrow: function.is_arrow,
            is_method: function.is_method,
            has_rest: function.has_rest,
            is_derived_constructor: function.is_derived_constructor,
            is_async: function.is_async,
            is_generator: function.is_generator,
            is_async_generator: function.is_async_generator,
            needs_arguments: function.needs_arguments,
            uses_arguments_callee: function.uses_arguments_callee,
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
                .map(|binding| ExecDirectEvalBinding {
                    name: binding.name.clone().into_boxed_str(),
                    upvalue: binding.upvalue,
                    lexical: binding.lexical,
                    captured: binding.captured,
                    is_const: binding.is_const,
                    fn_self_name: binding.fn_self_name,
                })
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
    reserved: [u8; 2],
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
            reserved: [0; 2],
        }
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

    fn function(code: Vec<Instruction>) -> Function {
        Function {
            id: 0,
            name: "exec-test".to_string(),
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
            constants: Vec::new(),
            module_resolutions: Vec::new(),
            module_inits: Vec::new(),
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
                bound_this_byte: gc_header_bytes
                    + crate::closure::CLOSURE_BODY_BOUND_THIS_OFFSET as u32,
                bound_new_target_byte: gc_header_bytes
                    + crate::closure::CLOSURE_BODY_BOUND_NEW_TARGET_OFFSET as u32,
                bound_this_flag: crate::closure::CLOSURE_CALL_FLAG_BOUND_THIS,
                bound_new_target_flag: crate::closure::CLOSURE_CALL_FLAG_BOUND_NEW_TARGET,
                runtime_setup_flags: crate::closure::CLOSURE_CALL_RUNTIME_SETUP_FLAGS,
            }
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
        let function = function(vec![
            Instruction {
                pc: 0,
                op: Op::LoadInt32,
                operands: vec![Operand::Register(u16::MAX), Operand::Imm32(i32::MIN)],
            },
            Instruction {
                pc: 1,
                op: Op::LoadNumber,
                operands: vec![Operand::Register(7), Operand::ConstIndex(u32::MAX)],
            },
        ]);
        let executable = ExecutableModule::from_bytecode(&module(function));
        let function = executable.function(0).unwrap();

        assert_eq!(function.register(&function.code[0], 0), Some(u16::MAX));
        assert_eq!(function.imm32(&function.code[0], 1), Some(i32::MIN));
        assert_eq!(function.register(&function.code[1], 0), Some(7));
        assert_eq!(function.const_index(&function.code[1], 1), Some(u32::MAX));
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
    fn named_property_ops_get_dense_ic_sites() {
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
        ]);
        let module = module(function);

        let executable = ExecutableModule::from_bytecode(&module);
        let function = executable.function(0).unwrap();

        assert_eq!(executable.property_ic_site_end(), 2);
        assert_eq!(function.code[0].property_ic_site(), Some(0));
        assert_eq!(function.code[1].property_ic_site(), Some(1));
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
        assert_eq!(view.instructions.len(), 2);
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
        assert_eq!(exec_fn.code.len(), 2);
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
