//! Frozen execution bytecode for the VM dispatch loop.
//!
//! `otter-bytecode` owns the compiler/debug DTO shape. The VM owns this
//! compact view so hot dispatch reads opcodes, operands, byte offsets,
//! and named-property IC sites directly off each instruction record.
//!
//! # Contents
//! - [`ExecutableModuleBuilder`] — transient builder over compiler bytecode.
//! - [`ExecutableModule`] — VM-owned frozen function table.
//! - [`ExecutableFunction`] — one function body: instruction stream,
//!   byte-stream length, byte-offset source-map spans.
//! - [`ExecInstr`] — single instruction record: opcode, owned operands,
//!   byte length, byte-offset PC, optional IC site id.
//!
//! # Invariants
//! - `frame.pc` is a byte offset into the function's encoded stream.
//! - Each `ExecInstr` carries its own `byte_pc` and `byte_len` so the
//!   dispatch loop advances by `byte_len` and resolves jump targets in the
//!   same coordinate system as the source-map spans.
//! - `ExecutableFunction::byte_to_instr` is a dense byte-offset → `code`
//!   index map, so PC resolution is `O(1)`, not a binary search.
//! - Operands live in a per-instruction `Box<[Operand]>`; there is no
//!   shared side table. Variadic opcodes just hold a longer slice.
//! - Branch-class `Imm32` operands hold byte-offset deltas relative to
//!   `(byte_pc + 1)`. `NO_HANDLER_OFFSET` is preserved verbatim for absent
//!   try-handler slots.
//! - Named property IC sites receive dense VM-local ids during build; the
//!   bytecode JSON dump stays unchanged.
//!
//! # See also
//! - [`crate::execution_context`]
//! - [`otter_bytecode::Instruction`]

use otter_bytecode::{
    ArgumentBindingStorage, ArgumentsObjectKind, BytecodeModule, Function, NO_HANDLER_OFFSET, Op,
    Operand, SpanEntry,
    encoding::{EncodedFunction, encode_function, translate_spans_to_byte_pcs},
};

const NO_PROPERTY_IC_SITE: u32 = u32::MAX;

/// Sentinel in [`ExecutableFunction::byte_to_instr`] for byte offsets
/// that are not an instruction boundary (interior bytes / past-end).
const NO_INSTR_AT_BYTE: u32 = u32::MAX;

/// Transient builder for [`ExecutableModule`].
///
/// The builder owns dense IC-site assignment while the VM creates an
/// [`crate::ExecutionContext`]. Dispatch receives only the frozen
/// [`ExecutableModule`] produced by [`Self::freeze`].
#[derive(Debug, Default)]
pub(crate) struct ExecutableModuleBuilder {
    functions: Vec<ExecutableFunction>,
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
            builder.push_function(function);
        }
        builder
    }

    fn push_function(&mut self, function: &Function) {
        let function = ExecutableFunction::from_bytecode(function, &mut self.next_property_ic_site);
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
    functions: Box<[ExecutableFunction]>,
    property_ic_site_end: u32,
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
    pub(crate) fn function(&self, local_index: u32) -> Option<&ExecutableFunction> {
        self.functions.get(local_index as usize)
    }

    /// Return an instruction's operands in declaration order.
    #[must_use]
    pub(crate) fn operands<'a>(&self, instr: &'a ExecInstr) -> &'a [Operand] {
        instr.operands()
    }

    /// Return one instruction operand by index without materialising the
    /// whole operand slice at the call site.
    #[must_use]
    pub(crate) fn operand<'a>(&self, instr: &'a ExecInstr, index: usize) -> Option<&'a Operand> {
        instr.operand(index)
    }

    /// Decode one register operand.
    #[must_use]
    pub(crate) fn register(&self, instr: &ExecInstr, index: usize) -> Option<u16> {
        instr.register(index)
    }

    /// Decode one constant-pool index operand.
    #[must_use]
    pub(crate) fn const_index(&self, instr: &ExecInstr, index: usize) -> Option<u32> {
        instr.const_index(index)
    }

    /// Decode one signed immediate operand.
    #[must_use]
    pub(crate) fn imm32(&self, instr: &ExecInstr, index: usize) -> Option<i32> {
        instr.imm32(index)
    }

    /// One past the highest dense named-property IC site id in this
    /// module (equals the site count when the IC base is zero).
    #[must_use]
    pub(crate) const fn property_ic_site_end(&self) -> u32 {
        self.property_ic_site_end
    }
}

/// Byte offset from a decompressed closure pointer to the *data pointer* of its
/// captured upvalue spine, for the inlined-closure `InlineUpvalue` lowering.
///
/// `Vec<T>` is three pointer-sized words but the field order (`ptr`, `cap`,
/// `len`) is not a stable ABI — the compiler may place the data pointer in any
/// of the three words. Rather than bake a guessed word, probe a freshly
/// allocated `Vec<UpvalueCell>` and find the word whose value equals
/// [`Vec::as_ptr`]; that offset is then constant for the running binary.
fn closure_upvalues_ptr_byte() -> u32 {
    let body_off = otter_gc::header::HEADER_SIZE
        + std::mem::offset_of!(crate::closure::JsClosureBody, upvalues);
    let probe: Vec<crate::UpvalueCell> = Vec::with_capacity(1);
    let base = std::ptr::from_ref(&probe) as usize;
    let want = probe.as_ptr() as usize;
    let word = (0..3)
        .map(|w| w * std::mem::size_of::<usize>())
        // SAFETY: `probe` is a live `Vec`, three pointer-sized words wide; each
        // in-range word is a valid `usize` read.
        .find(|&off| unsafe { *((base + off) as *const usize) } == want)
        .expect("Vec<UpvalueCell> data pointer not in first three words");
    (body_off + word) as u32
}

impl ExecutableFunction {
    /// Build an owned JIT compile-input snapshot for this function.
    #[must_use]
    pub(crate) fn jit_view(&self) -> crate::jit::JitFunctionView {
        crate::jit::JitFunctionView {
            function_id: self.id,
            param_count: self.param_count,
            register_count: self.register_count,
            code_byte_len: self.code_byte_len,
            is_strict: self.is_strict,
            is_async: self.is_async,
            is_generator: self.is_generator,
            is_async_generator: self.is_async_generator,
            // Baked by `Interpreter::compile_jit_function`, which holds the
            // cage base and the live property-IC tables.
            cage_base: 0,
            // Baked alongside `cage_base` by `compile_jit_function`; the
            // all-zero default is never read because the emitter gates inline
            // element access on `cage_base != 0`.
            ta_layout: crate::jit::JitTypedArrayLayout::default(),
            string_layout: crate::jit::JitStringLayout::default(),
            // `#[repr(C)]` constant: offset from the decompressed object
            // pointer to its shape handle, for the WhiskerIC load-cell guard.
            object_shape_byte: otter_gc::header::HEADER_SIZE as u32
                + crate::object::OBJECT_BODY_SHAPE_OFFSET as u32,
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
            closure_fid_byte: otter_gc::header::HEADER_SIZE as u32
                + std::mem::offset_of!(crate::closure::JsClosureBody, function_id) as u32,
            closure_upvalues_ptr_byte: closure_upvalues_ptr_byte(),
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
                .map(|instr| crate::jit::JitInstrView {
                    op: instr.op(),
                    byte_pc: instr.byte_pc,
                    byte_len: instr.byte_len(),
                    property_ic_site: instr.property_ic_site(),
                    operands: instr.operands().to_vec(),
                    // Resolved by `ExecutionContext::jit_function_view`, which
                    // can map a `MakeFunction` constant index to its target id.
                    make_self: false,
                    // Resolved by `ExecutionContext::jit_function_view`, which
                    // can inspect constant strings without exposing them to the
                    // external JIT crate.
                    load_array_length: false,
                    method_hint: crate::jit::JitMethodHint::None,
                    // Resolved by `ExecutionContext::jit_function_view`, which
                    // can read the number-constant pool for a `LoadNumber`.
                    load_number: None,
                    // Baked by `Interpreter::bake_arith_feedback` from the live
                    // per-site warmup feedback; the raw snapshot carries none.
                    arith_feedback: 0,
                    // Baked by `Interpreter::bake_property_feedback` from the
                    // live per-site property IC; the raw snapshot carries none.
                    property_feedback: None,
                    property_feedback_poly: Vec::new(),
                    property_proto_feedback: None,
                    object_literal: None,
                    // Baked by `Interpreter::bake_element_load_kind` from the
                    // live per-site warmup feedback; the raw snapshot carries none.
                    element_load_kind: crate::jit::JitElementLoadKind::Any,
                    // Baked by `Interpreter::bake_global_lex_cells` once the
                    // lexical binding exists; the raw snapshot carries none.
                    global_lex_cell: None,
                })
                .collect(),
            // Baked by `Interpreter::bake_inline_callees` (it holds the live
            // per-site feedback and can resolve callee bodies); the raw snapshot
            // carries none.
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

    /// Byte-offset source-map entries, sorted by `pc`. Empty when the
    /// underlying [`Function::spans`] is empty.
    #[must_use]
    pub(crate) fn byte_spans(&self) -> &[SpanEntry] {
        &self.byte_spans
    }

    /// Total length in bytes of this function's encoded stream.
    #[must_use]
    #[allow(dead_code)]
    pub(crate) const fn code_byte_len(&self) -> u32 {
        self.code_byte_len
    }

    /// Resolve a byte-offset PC to its `ExecInstr` in `O(1)` via the
    /// dense `byte_to_instr` boundary map. Returns `None` when `byte_pc`
    /// is out of range or does not fall on an instruction boundary (which
    /// only happens on corrupt bytecode).
    #[must_use]
    pub(crate) fn instr_at_byte_pc(&self, byte_pc: u32) -> Option<&ExecInstr> {
        let idx = *self.byte_to_instr.get(byte_pc as usize)?;
        if idx == NO_INSTR_AT_BYTE {
            return None;
        }
        self.code.get(idx as usize)
    }

    /// Resolve a byte-offset PC to its dense `code` index via the
    /// `byte_to_instr` boundary map. The dispatch loop caches this index
    /// per frame and advances it by one on straight-line ticks, so the
    /// `byte_pc` → index lookup is paid only on entry, branches, and
    /// call/return — not on every instruction. Returns `None` on the same
    /// corrupt-bytecode conditions as [`Self::instr_at_byte_pc`].
    #[must_use]
    pub(crate) fn instr_index_at_byte_pc(&self, byte_pc: u32) -> Option<usize> {
        let idx = *self.byte_to_instr.get(byte_pc as usize)?;
        if idx == NO_INSTR_AT_BYTE {
            return None;
        }
        (idx as usize).lt(&self.code.len()).then_some(idx as usize)
    }

    /// Fetch an instruction by its dense `code` index.
    #[must_use]
    pub(crate) fn instr_at_index(&self, index: usize) -> Option<&ExecInstr> {
        self.code.get(index)
    }
}

/// One executable function body.
#[derive(Debug, Clone)]
pub(crate) struct ExecutableFunction {
    /// Global VM function id (chunk base + local table index).
    pub(crate) id: u32,
    /// Number of parameter registers at the start of the frame.
    pub(crate) param_count: u16,
    /// Total register window size: params + locals + scratch.
    pub(crate) register_count: u16,
    /// Number of fresh upvalue cells owned by each frame.
    pub(crate) own_upvalue_count: u16,
    /// `true` when this function uses strict-mode call semantics.
    pub(crate) is_strict: bool,
    /// `true` when this function is an arrow function.
    pub(crate) is_arrow: bool,
    /// `true` when this function is a MethodDefinition body (class
    /// or object-literal method / accessor) — never a constructor,
    /// carries no implicit `prototype` property.
    pub(crate) is_method: bool,
    /// `true` when this function declares a rest parameter.
    pub(crate) has_rest: bool,
    /// `true` when this function is async.
    pub(crate) is_async: bool,
    /// `true` when this function is a generator.
    pub(crate) is_generator: bool,
    /// `true` when this function is an async generator.
    pub(crate) is_async_generator: bool,
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
    /// `true` when this function body makes no nested JS call / construct — a
    /// leaf. The frameless direct-method call (register-CC window, no HoltStack
    /// frame) is only sound for a leaf callee: a nested call reads
    /// `JitCtx.frame_index` to find its own frame's registers, but a frameless
    /// callee has no such frame, so a non-leaf callee must stay on the bridge.
    pub(crate) is_leaf: bool,
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
    /// Hot instruction stream. Indexed in source order; the dispatch
    /// loop resolves a frame's byte-offset PC to an entry via
    /// [`Self::instr_at_byte_pc`] (`O(1)` lookup through `byte_to_instr`).
    pub(crate) code: Box<[ExecInstr]>,
    /// Dense byte-offset → `code` index map (length `code_byte_len`).
    /// Instruction-boundary bytes hold the entry's index; interior bytes
    /// hold [`NO_INSTR_AT_BYTE`]. Turns PC resolution into a single array
    /// index instead of an `O(log N)` binary search over `code`. Costs
    /// `4 × code_byte_len` bytes per function — paid once at build.
    pub(crate) byte_to_instr: Box<[u32]>,
    /// Source-map entries with `pc` expressed as a byte offset into the
    /// encoded stream. Empty when the underlying [`Function::spans`] is empty.
    pub(crate) byte_spans: Box<[SpanEntry]>,
    /// Total length in bytes of this function's encoded stream. Acts as
    /// the upper bound for jump targets that fall off the end of the
    /// last instruction.
    pub(crate) code_byte_len: u32,
}

impl ExecutableFunction {
    fn from_bytecode(function: &Function, next_property_ic_site: &mut u32) -> Self {
        let register_count = function
            .param_count
            .saturating_add(function.locals)
            .saturating_add(function.scratch);
        let EncodedFunction {
            code: code_bytes,
            instr_to_byte_pc,
        } = encode_function(&function.code);
        let code_byte_len =
            u32::try_from(code_bytes.len()).expect("function byte stream exceeds u32 range");
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
                let byte_pc = instr_to_byte_pc[idx];
                let next_byte_pc = instr_to_byte_pc
                    .get(idx + 1)
                    .copied()
                    .unwrap_or(code_byte_len);
                let byte_len = u16::try_from(next_byte_pc - byte_pc)
                    .expect("single instruction exceeds 65535-byte encoding");
                // Compiler emits branch deltas in instruction-index units;
                // the dispatcher resolves them as byte offsets relative to
                // `(byte_pc + 1)` (the byte right after the opcode), so the
                // executable builder rewrites each branch operand into the
                // dispatcher's coordinate system here.
                let operands = rewrite_branch_operands(
                    instr.op,
                    instr.operands.as_slice(),
                    idx,
                    byte_pc,
                    &instr_to_byte_pc,
                    code_byte_len,
                );
                ExecInstr::from_operands(instr.op, operands, property_ic_site, byte_pc, byte_len)
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let mapped_argument_bindings = function
            .mapped_argument_bindings
            .iter()
            .map(|binding| ExecMappedArgumentBinding {
                argument_index: binding.argument_index,
                storage: binding.storage,
            })
            .collect();
        // Invert `instr_to_byte_pc` into a dense byte → index map so the
        // dispatch loop resolves a byte-offset PC in O(1). Interior /
        // past-end bytes stay `NO_INSTR_AT_BYTE`.
        let mut byte_to_instr = vec![NO_INSTR_AT_BYTE; code_byte_len as usize];
        for (idx, &bpc) in instr_to_byte_pc.iter().enumerate() {
            byte_to_instr[bpc as usize] = idx as u32;
        }
        let byte_to_instr = byte_to_instr.into_boxed_slice();
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
        // A leaf body makes no nested JS call / construct. Conservative: any
        // call- or construct-style opcode (including native builtin calls that
        // may re-enter the VM) disqualifies it. Used to gate the frameless
        // register-CC direct-method call to callees that never need a frame.
        let is_leaf = !function.code.iter().any(|instr| {
            matches!(
                instr.op,
                Op::Call
                    | Op::CallMethodValue
                    | Op::CallWithThis
                    | Op::CallSpread
                    | Op::TailCall
                    | Op::New
                    | Op::NewSpread
                    | Op::SuperConstructSpread
                    | Op::PromiseCall
                    | Op::PromiseNew
                    | Op::MathCall
                    | Op::BigIntCall
                    | Op::ArrayBufferCall
                    | Op::SharedArrayBufferCall
                    | Op::DataViewCall
                    | Op::ArrayConstruct
            )
        });
        Self {
            id: function.id,
            makes_function,
            is_leaf,
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
            module_url: function.module_url.clone().into_boxed_str(),
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
            byte_to_instr,
            byte_spans,
            code_byte_len,
        }
    }
}

/// Translate branch-class `Imm32` operands from compiler-emitted
/// instruction-index deltas into byte-offset deltas relative to
/// `(jump_byte_pc + 1)`. Non-branch opcodes pass through.
fn rewrite_branch_operands(
    op: Op,
    operands: &[Operand],
    jump_idx: usize,
    jump_byte_pc: u32,
    instr_to_byte_pc: &[u32],
    code_byte_len: u32,
) -> Vec<Operand> {
    let branch_slots: &[usize] = match op {
        Op::Jump | Op::JumpIfTrue | Op::JumpIfFalse | Op::JumpIfNullish => &[0],
        // `JumpViaFinally` operand 0 is the branch delta; operand 1 is
        // the handler-stack floor (not a branch target).
        Op::JumpViaFinally => &[0],
        Op::EnterTry => &[0, 1],
        _ => return operands.to_vec(),
    };
    let mut out = operands.to_vec();
    for &slot in branch_slots {
        let Some(Operand::Imm32(raw)) = out.get(slot).copied() else {
            continue;
        };
        if raw == NO_HANDLER_OFFSET {
            continue;
        }
        let target_idx = jump_idx as i64 + 1 + raw as i64;
        let target_byte_pc = if target_idx as usize >= instr_to_byte_pc.len() {
            code_byte_len
        } else {
            instr_to_byte_pc[target_idx as usize]
        };
        let byte_delta = i64::from(target_byte_pc) - (i64::from(jump_byte_pc) + 1);
        let byte_delta_i32 =
            i32::try_from(byte_delta).expect("branch byte-offset delta exceeds i32 range");
        out[slot] = Operand::Imm32(byte_delta_i32);
    }
    out
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

/// Hot dispatch instruction. Owns its operand slice so dispatch only
/// touches the instruction record and the per-instruction operand
/// allocation; there is no module-level side table to chase through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExecInstr {
    /// Opcode.
    op: Op,
    /// Byte length of this instruction in the encoded stream
    /// (`opcode` + `operand_count` header + tagged operand bytes).
    /// `u16` to cover pathological inputs (constant-pool indices that
    /// occupy multiple varint bytes per operand combined with
    /// variadic opcodes) — a single instruction can encode up to
    /// ~640 bytes for `NewArray` over thousands of literals.
    byte_len: u16,
    /// Dense module-local property IC site id for named property ops.
    property_ic_site: u32,
    /// Byte-offset PC of this instruction in the encoded stream.
    byte_pc: u32,
    /// Operands in declaration order. Variadic opcodes (e.g. `Call`,
    /// `NewArray`, `MakeClosure`) just lengthen the slice; there is no
    /// fixed-width inline fast path.
    operands: Box<[Operand]>,
}

impl ExecInstr {
    fn from_operands(
        op: Op,
        operands: Vec<Operand>,
        property_ic_site: u32,
        byte_pc: u32,
        byte_len: u16,
    ) -> Self {
        Self {
            op,
            byte_len,
            property_ic_site,
            byte_pc,
            operands: operands.into_boxed_slice(),
        }
    }

    /// Opcode.
    #[must_use]
    pub(crate) const fn op(&self) -> Op {
        self.op
    }

    /// Byte length of this instruction in the encoded stream.
    #[must_use]
    pub(crate) const fn byte_len(&self) -> u32 {
        self.byte_len as u32
    }

    /// Dense property IC site index for named property opcodes.
    #[must_use]
    pub(crate) const fn property_ic_site(&self) -> Option<usize> {
        if self.property_ic_site == NO_PROPERTY_IC_SITE {
            None
        } else {
            Some(self.property_ic_site as usize)
        }
    }

    fn operands(&self) -> &[Operand] {
        &self.operands
    }

    fn operand(&self, index: usize) -> Option<&Operand> {
        self.operands.get(index)
    }

    fn register(&self, index: usize) -> Option<u16> {
        match self.operand(index) {
            Some(Operand::Register(reg)) => Some(*reg),
            _ => None,
        }
    }

    fn const_index(&self, index: usize) -> Option<u32> {
        match self.operand(index) {
            Some(Operand::ConstIndex(idx)) => Some(*idx),
            _ => None,
        }
    }

    fn imm32(&self, index: usize) -> Option<i32> {
        match self.operand(index) {
            Some(Operand::Imm32(value)) => Some(*value),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_bytecode::{BytecodeModule, Instruction, SourceKind};

    fn function(code: Vec<Instruction>) -> Function {
        Function {
            id: 0,
            name: "exec-test".to_string(),
            code,
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
    fn fixed_width_operands_stay_inline() {
        let function = function(vec![Instruction {
            pc: 99,
            op: Op::Add,
            operands: vec![
                Operand::Register(0),
                Operand::Register(1),
                Operand::Register(2),
            ]
            .into(),
        }]);
        let module = module(function);

        let executable = ExecutableModule::from_bytecode(&module);
        let instr = &executable.function(0).unwrap().code[0];

        assert_eq!(instr.op(), Op::Add);
        assert_eq!(executable.register(instr, 0), Some(0));
        assert_eq!(executable.register(instr, 1), Some(1));
        assert_eq!(executable.register(instr, 2), Some(2));
        assert_eq!(executable.register(instr, 3), None);
        assert_eq!(
            executable.operands(instr),
            &[
                Operand::Register(0),
                Operand::Register(1),
                Operand::Register(2)
            ]
        );
    }

    #[test]
    fn variadic_operands_are_owned_by_the_instruction() {
        let operands = vec![
            Operand::Register(0),
            Operand::Register(1),
            Operand::ConstIndex(4),
            Operand::Register(2),
            Operand::Register(3),
        ];
        let function = function(vec![Instruction {
            pc: 7,
            op: Op::Call,
            operands: operands.clone().into(),
        }]);
        let module = module(function);

        let executable = ExecutableModule::from_bytecode(&module);
        let instr = &executable.function(0).unwrap().code[0];

        assert_eq!(instr.op(), Op::Call);
        assert_eq!(executable.register(instr, 0), Some(0));
        assert_eq!(executable.register(instr, 1), Some(1));
        assert_eq!(executable.const_index(instr, 2), Some(4));
        assert_eq!(executable.register(instr, 3), Some(2));
        assert_eq!(executable.register(instr, 4), Some(3));
        assert_eq!(executable.register(instr, 5), None);
        assert_eq!(executable.operands(instr), operands.as_slice());
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
                ]
                .into(),
            },
            Instruction {
                pc: 1,
                op: Op::StoreProperty,
                operands: vec![
                    Operand::Register(1),
                    Operand::ConstIndex(0),
                    Operand::Register(0),
                    Operand::Register(2),
                ]
                .into(),
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
    fn jit_view_carries_bytecode_and_ic_metadata() {
        let function = function(vec![
            Instruction {
                pc: 0,
                op: Op::LoadProperty,
                operands: vec![
                    Operand::Register(0),
                    Operand::Register(1),
                    Operand::ConstIndex(7),
                ]
                .into(),
            },
            Instruction {
                pc: 1,
                op: Op::Add,
                operands: vec![
                    Operand::Register(2),
                    Operand::Register(0),
                    Operand::Register(3),
                ]
                .into(),
            },
        ]);
        let module = module(function);

        let executable = ExecutableModule::from_bytecode(&module);
        let view = executable.function(0).unwrap().jit_view();

        assert_eq!(view.function_id, 0);
        assert_eq!(view.instructions.len(), 2);
        assert_eq!(view.instructions[0].op, Op::LoadProperty);
        assert_eq!(view.instructions[0].byte_pc, 0);
        assert!(view.instructions[0].byte_len > 0);
        assert_eq!(view.instructions[0].property_ic_site, Some(0));
        assert_eq!(
            view.instructions[0].operands,
            vec![
                Operand::Register(0),
                Operand::Register(1),
                Operand::ConstIndex(7),
            ]
        );
        assert_eq!(view.instructions[1].op, Op::Add);
        assert_eq!(view.instructions[1].property_ic_site, None);
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
                ]
                .into(),
            },
            Instruction {
                pc: 1,
                op: Op::Call,
                operands: vec![
                    Operand::Register(2),
                    Operand::Register(3),
                    Operand::ConstIndex(4),
                    Operand::Register(5),
                ]
                .into(),
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
            executable.operands(&exec_fn.code[1]),
            &[
                Operand::Register(2),
                Operand::Register(3),
                Operand::ConstIndex(4),
                Operand::Register(5)
            ]
        );
    }
}
