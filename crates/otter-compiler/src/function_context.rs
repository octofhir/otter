//! Per-function bytecode emission state: registers, scopes, contexts, and
//! instruction emission.
//!
//! A function's bindings live in registers or in context slots. Each
//! compile-time [`Scope`] owns a context once it declares its first slot (or
//! is forced to anchor a sloppy direct eval's extension); the context is a
//! register filled by `CreateContext` whose parent is the innermost enclosing
//! context of the same function, or the function's closure context.
//!
//! # Contents
//! - [`FunctionContext`] — per-function emission state (its scope entry,
//!   context creation, and binding declaration live in `scope`).
//! - closure-context operands: patched to a dedicated register filled by a
//!   prologue `LoadClosureContext` at finalization
//! - constant interning and jump patching
//! - [`FinishedCode`] — the finalized wordcode, spans, and scope table
//!
//! # Invariants
//! - Instruction spans are emitted alongside bytecode positions.
//! - A context is created only for the innermost scope, so its parent chain
//!   is fixed at creation and matches every later static depth computation.
//! - Plain uncaptured formals may bind their incoming ABI register directly;
//!   all later scratch allocation starts above the reserved argument window.
//! - Storing a value into its existing register is a bytecode no-op.
//! - A derived constructor whose `this` lives in a `DerivedThis` slot
//!   completes only through `ReturnDerived`: every return emitted into such a
//!   frame is rewritten at emission.
//! - The closure-context register is chosen at finalization above every other
//!   register, so no temporary ever aliases it.
//!
//! # See also
//! - `scope` for scope and binding records.
//! - `compiler` for cross-frame resolution.

use crate::scope::{CtxReg, ScopeContext};
use crate::*;
use otter_bytecode::{ContextCoord, ScopeDescriptor};

/// The `DerivedThis` slot of a derived constructor frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DerivedThisSlot {
    /// Register holding the parameter-scope context that owns the slot.
    pub(crate) ctx: u16,
    /// Slot index.
    pub(crate) slot: u16,
}

/// Finalized output of one function body.
#[derive(Debug)]
pub(crate) struct FinishedCode {
    pub(crate) code: otter_bytecode::FunctionCode,
    pub(crate) spans: Vec<SpanEntry>,
    pub(crate) scratch: u16,
    pub(crate) scopes: Vec<ScopeDescriptor>,
    /// The body reads its closure context (`LoadClosureContext` prologue).
    pub(crate) uses_closure_context: bool,
    pub(crate) number_hint_sites: Vec<u32>,
    pub(crate) class_hint_sites: Vec<(u32, u32)>,
    pub(crate) handlers: Vec<otter_bytecode::ExceptionHandler>,
}

/// Per-function compilation context.
#[derive(Debug)]
pub(crate) struct FunctionContext {
    pub(crate) module: Rc<RefCell<ModuleBuilder>>,
    pub(crate) code: FunctionCodeBuilder,
    pub(crate) spans: Vec<SpanEntry>,
    pub(crate) scratch: u16,
    /// Stack of lexical scopes, outermost first.
    pub(crate) scopes: Vec<Scope>,
    /// Index into [`Self::scopes`] of the function's VariableEnvironment
    /// scope (where hoisted `var` and function declarations live).
    pub(crate) var_scope: usize,
    /// Scope descriptors of every context this function creates, indexed
    /// by `CreateContext`'s scope operand.
    pub(crate) scope_descriptors: Vec<ScopeDescriptor>,
    /// Operand positions `(pc, operand index)` that name the closure
    /// context; patched to one dedicated register at finalization.
    pub(crate) closure_ctx_patches: Vec<(u32, usize)>,
    /// The derived-constructor `this` slot, when this frame keeps `this`
    /// in its parameter-scope context.
    pub(crate) derived_this: Option<DerivedThisSlot>,
    /// Register holding the context of the mapped formals a sloppy
    /// arguments object aliases, named by `CallForwardArguments`.
    pub(crate) mapped_arguments_ctx: Option<u16>,
    /// The function's closure context is statically `undefined`: a script
    /// `<main>`, an indirect eval `<main>`, or a function created without
    /// a context. A closure-context operand then reads a register holding
    /// `undefined` instead of loading the closure context.
    pub(crate) closure_context_empty: bool,
    /// ECMAScript strictness for the function currently being
    /// lowered. This is compile-time metadata stored on the
    /// resulting bytecode function and also drives early errors.
    pub(crate) is_strict: bool,
    /// `true` when this context lowers an arrow function. Arrows have
    /// no own `arguments` binding, which changes the
    /// EvalDeclarationInstantiation `var arguments` early error
    /// (§19.2.1.3) for direct eval call sites inside the body.
    pub(crate) is_arrow: bool,
    /// `true` for a direct-eval `<main>` whose `this` is the caller's
    /// derived-constructor `DerivedThis` slot (the eval may call
    /// `super()`): `this` resolves through the caller chain like an arrow.
    pub(crate) eval_this_from_chain: bool,
    /// The frame carries a [[HomeObject]] — MethodDefinition bodies,
    /// class constructors, and static blocks. `super.x` inside a
    /// direct eval is legal only when the innermost non-arrow frame
    /// has one (§19.2.1.1).
    pub(crate) has_home_object: bool,
    /// The frame is a DERIVED class constructor — `super()` inside a
    /// direct eval is legal only when the innermost non-arrow frame
    /// is one (§19.2.1.1 direct-eval SuperCall).
    pub(crate) is_derived_ctor: bool,
    /// `true` when `super.x` in this context resolves its
    /// [[HomeObject]] through the class STATICS side: static methods /
    /// accessors, static blocks, and static field initializers. Arrows
    /// inherit the flag lexically from their enclosing context.
    pub(crate) super_home_static: bool,
    /// `true` while formal-parameter defaults of this function are
    /// being lowered. A direct eval in that window var-declaring
    /// `arguments` is an early SyntaxError when [`Self::binds_arguments`]
    /// holds (§19.2.1.3).
    pub(crate) in_param_init: bool,
    /// `true` when this function will have an `arguments` binding in
    /// its variable environment.
    pub(crate) binds_arguments: bool,
    /// `true` when every `arguments` reference in this body is the
    /// forwarded list of `<callee>.apply(<this>, arguments)`.
    pub(crate) arguments_forward_only: bool,
    /// `true` when the compilation unit reads a `.arguments` property
    /// somewhere. Inherited by every nested function.
    pub(crate) dot_arguments_observed: bool,
    /// Canonical source URL inherited by nested functions.
    pub(crate) module_url: String,
    pub(crate) is_async_generator: bool,
    /// §15.10.2 — a call in tail position of this body replaces its
    /// activation: the body is neither a generator nor an async body.
    /// Strictness is checked separately.
    pub(crate) proper_tail_calls: bool,
    /// Stack of enclosing loops; the innermost is on top.
    pub(crate) loops: Vec<LoopFrame>,
    /// Constructs an abrupt completion from the current point passes,
    /// innermost last.
    pub(crate) control: Vec<crate::control::ControlScope>,
    /// Exception handler table, innermost first.
    pub(crate) handlers: Vec<otter_bytecode::ExceptionHandler>,
    /// Label deposited by the immediately-enclosing
    /// `LabeledStatement` waiting to be consumed by the next pushed
    /// loop / switch frame.
    pub(crate) pending_label: Option<String>,
    /// Names that a scope-entry pre-pass already compiled + stored as
    /// hoisted function declarations.
    pub(crate) hoisted_function_names: HashSet<String>,
    /// §B.3.3 — block-level function names receiving the sloppy-mode
    /// var-scope extension, mapped to the variable-scope target the
    /// declaration's source position syncs into. The `bool` marks
    /// global-script bindings that also mirror via `DefineGlobalVar`.
    pub(crate) annex_b_var_targets:
        std::collections::HashMap<String, (Option<crate::compiler::VarTarget>, bool)>,
    /// Source span starts of block-level function declarations whose
    /// own path is Annex B eligible.
    pub(crate) annex_b_eligible_spans: std::collections::HashSet<u32>,
    /// `true` when an anonymous `export default function/function*` was
    /// already hoisted at instantiation.
    pub(crate) default_function_hoisted: bool,
    /// Names of this function's own bindings that some nested function
    /// references or a direct eval may see — populated by the capture
    /// pre-pass. A binding with such a name lives in a context slot.
    pub(crate) captured_names: HashSet<String>,
    /// Simple formal names that must live in context slots so a sloppy
    /// mapped arguments object can alias them.
    pub(crate) mapped_argument_names: HashSet<String>,
    /// `Some` when this context is the top-level `<module-init>` of an
    /// ES-module fragment.
    pub(crate) module_state: Option<ModuleState>,
    /// `with` object environments enclosing the code being lowered.
    pub(crate) active_with_envs: Vec<crate::with_statement::WithEnv>,
    /// Set when `alloc_scratch` exhausted the u16 register window.
    pub(crate) register_overflow: bool,
    /// High-water mark of `alloc_scratch`.
    pub(crate) scratch_peak: u16,
    /// `true` when this function's own code contains a direct-eval call
    /// site.
    pub(crate) contains_direct_eval: bool,
    /// §15.7.1 — class heritage / computed keys lowered inline into a
    /// sloppy frame use the `*Strict` property-store opcodes.
    pub(crate) strict_class_parts: bool,
    /// §8.4 / §14 — the script / eval `<main>` completion-value register.
    pub(crate) completion_reg: Option<u16>,
    /// `true` while lowering a `finally` block body.
    pub(crate) completion_suppressed: bool,
    /// Instruction PCs whose operands are statically `number`.
    pub(crate) number_hint_sites: Vec<u32>,
    /// Property sites whose receiver is a class-annotated binding.
    pub(crate) class_hint_sites: Vec<(u32, u32)>,
}

impl FunctionContext {
    pub(crate) fn new(module: Rc<RefCell<ModuleBuilder>>) -> Self {
        Self {
            module,
            code: FunctionCodeBuilder::new(),
            spans: Vec::new(),
            scratch: 0,
            scopes: Vec::new(),
            var_scope: 0,
            scope_descriptors: Vec::new(),
            closure_ctx_patches: Vec::new(),
            derived_this: None,
            mapped_arguments_ctx: None,
            closure_context_empty: false,
            is_strict: false,
            is_arrow: false,
            eval_this_from_chain: false,
            has_home_object: false,
            is_derived_ctor: false,
            super_home_static: false,
            in_param_init: false,
            binds_arguments: false,
            dot_arguments_observed: false,
            module_url: String::new(),
            is_async_generator: false,
            proper_tail_calls: false,
            loops: Vec::new(),
            control: Vec::new(),
            handlers: Vec::new(),
            pending_label: None,
            hoisted_function_names: HashSet::new(),
            annex_b_var_targets: std::collections::HashMap::new(),
            annex_b_eligible_spans: std::collections::HashSet::new(),
            default_function_hoisted: false,
            captured_names: HashSet::new(),
            arguments_forward_only: false,
            mapped_argument_names: HashSet::new(),
            module_state: None,
            active_with_envs: Vec::new(),
            register_overflow: false,
            scratch_peak: 0,
            contains_direct_eval: false,
            strict_class_parts: false,
            completion_reg: None,
            completion_suppressed: false,
            number_hint_sites: Vec::new(),
            class_hint_sites: Vec::new(),
        }
    }

    pub(crate) fn with_strict(mut self, is_strict: bool) -> Self {
        self.is_strict = is_strict;
        self
    }

    pub(crate) fn with_arrow(mut self) -> Self {
        self.is_arrow = true;
        self
    }

    pub(crate) fn with_module_url(mut self, module_url: impl Into<String>) -> Self {
        self.module_url = module_url.into();
        self
    }

    /// Whether a binding named `name` declared in this function must live
    /// in a context slot.
    pub(crate) fn name_needs_slot(&self, name: &str) -> bool {
        self.captured_names.contains(name) || self.mapped_argument_names.contains(name)
    }

    pub(crate) fn alloc_scratch(&mut self) -> u16 {
        let r = self.scratch;
        match self.scratch.checked_add(1) {
            Some(next) => {
                self.scratch = next;
                if next > self.scratch_peak {
                    self.scratch_peak = next;
                }
            }
            None => {
                // Defer to a CompileError at function finalization —
                // a pathological source (tens of thousands of live
                // registers) must not abort the process.
                self.register_overflow = true;
            }
        }
        r
    }

    /// Free every temporary register at or above `mark`, rolling the
    /// scratch watermark back to `mark`. Expression combinators capture
    /// `mark = self.scratch` on entry, evaluate their operands (which
    /// bump scratch upward), then — immediately before emitting the
    /// single result-producing instruction — call this and allocate the
    /// destination at `mark`, so sibling subexpressions reuse the same
    /// low register range instead of stacking new ones.
    ///
    /// Sound because (a) expression lowering never declares a persistent
    /// binding — those are statement-level and always sit below `mark` —
    /// and the watermark never drops below the context register of an
    /// active scope, and (b) every result opcode reads all of its source
    /// operands before writing its destination.
    pub(crate) fn reset_scratch(&mut self, mark: u16) {
        debug_assert!(
            mark <= self.scratch,
            "reset_scratch above current watermark"
        );
        self.scratch_peak = self.scratch_peak.max(self.scratch);
        // A context created since `mark` belongs to a scope that is still
        // active; its register stays reserved.
        self.scratch = mark.max(self.context_register_floor()).min(self.scratch);
    }

    /// Final register-window size: the high-water mark survives
    /// scratch recycling.
    pub(crate) fn scratch_window(&self) -> u16 {
        self.scratch.max(self.scratch_peak)
    }

    /// Lowest register a statement may release down to without dropping
    /// a live scope context: one above the highest context register of
    /// every active scope.
    pub(crate) fn context_register_floor(&self) -> u16 {
        self.scopes
            .iter()
            .filter_map(|scope| match scope.context {
                Some(ScopeContext {
                    reg: CtxReg::Reg(reg),
                    ..
                }) => Some(reg.saturating_add(1)),
                _ => None,
            })
            .max()
            .unwrap_or(0)
    }

    /// Push `frame` onto the loop stack, consuming any pending
    /// `LabeledStatement` label.
    pub(crate) fn push_loop_frame(&mut self, mut frame: LoopFrame) {
        if frame.label.is_none() {
            frame.label = self.pending_label.take();
        }
        frame.break_depth = self.control.len();
        frame.continue_depth = self.control.len();
        self.loops.push(frame);
    }

    /// Attach a static type hint to the innermost binding of `name`.
    pub(crate) fn annotate_binding(&mut self, name: &str, hint: TypeHint) {
        if hint == TypeHint::Unknown {
            return;
        }
        for scope in self.scopes.iter_mut().rev() {
            if let Some(info) = scope.bindings.get_mut(name) {
                info.type_hint = hint;
                return;
            }
        }
    }

    /// Mark the next emitted instruction as statically `number`-typed on both
    /// operands. Call immediately before the [`Self::emit`] it describes.
    pub(crate) fn mark_number_hint_site(&mut self) {
        let pc = self.next_pc();
        self.number_hint_sites.push(pc);
    }

    /// Mark the next emitted instruction as a property access on a receiver
    /// annotated with the interned class name `name`.
    pub(crate) fn mark_class_hint_site(&mut self, name: u32) {
        let pc = self.next_pc();
        self.class_hint_sites.push((pc, name));
    }

    /// Emit a placeholder branch and return its instruction index
    /// so a later [`Self::patch_branch`] can fill in the offset.
    pub(crate) fn emit_branch_placeholder(
        &mut self,
        op: Op,
        cond_reg: Option<u16>,
        span: (u32, u32),
    ) -> u32 {
        let pc = self.next_pc();
        let operands = if let Some(reg) = cond_reg {
            vec![Operand::Imm32(0), Operand::Register(reg)]
        } else {
            vec![Operand::Imm32(0)]
        };
        self.code.push(op, operands.as_slice());
        self.spans.push(SpanEntry { pc, span });
        pc
    }

    /// Patch a previously emitted branch so it targets the
    /// **current** `next_pc`.
    pub(crate) fn patch_branch_to_here(&mut self, branch_pc: u32) {
        let target = self.next_pc();
        self.patch_branch(branch_pc, target);
    }

    /// Patch a previously emitted branch to point at `target_pc`.
    pub(crate) fn patch_branch(&mut self, branch_pc: u32, target_pc: u32) {
        let offset = target_pc as i64 - (branch_pc as i64 + 1);
        let offset = i32::try_from(offset).expect("branch offset out of i32 range");
        assert!(
            self.code.set_operand(branch_pc, 0, Operand::Imm32(offset)),
            "patch target operand not Imm32"
        );
    }

    /// Append `values` as one contiguous run of string constants and return
    /// the index of the first.
    pub(crate) fn push_string_constant_run(&mut self, values: &[&str]) -> u32 {
        let mut module = self.module.borrow_mut();
        let first = module.constants.len() as u32;
        for value in values {
            module.push_constant(Constant::String {
                utf16: value.encode_utf16().collect(),
            });
        }
        first
    }

    pub(crate) fn intern_string_constant(&mut self, value: &str) -> u32 {
        let utf16: Vec<u16> = value.encode_utf16().collect();
        self.intern_utf16_string_constant(utf16)
    }

    /// Intern a pre-built WTF-16 unit vector.
    pub(crate) fn intern_utf16_string_constant(&mut self, utf16: Vec<u16>) -> u32 {
        self.module
            .borrow_mut()
            .intern_constant(Constant::String { utf16 })
    }

    pub(crate) fn intern_number_constant(&mut self, value: f64) -> u32 {
        self.module.borrow_mut().intern_constant(Constant::Number {
            bits: value.to_bits(),
        })
    }

    pub(crate) fn intern_bigint_constant(&mut self, decimal: &str) -> u32 {
        self.module.borrow_mut().intern_constant(Constant::BigInt {
            decimal: decimal.to_string(),
        })
    }

    pub(crate) fn intern_regexp_constant(&mut self, pattern_utf16: &[u16], flags: &str) -> u32 {
        self.module.borrow_mut().intern_constant(Constant::RegExp {
            pattern_utf16: pattern_utf16.to_vec(),
            flags: flags.to_string(),
        })
    }

    pub(crate) fn intern_function_id(&mut self, function_id: u32) -> u32 {
        self.module
            .borrow_mut()
            .intern_constant(Constant::FunctionId { index: function_id })
    }

    /// Emit one instruction. A return emitted into a derived constructor
    /// that keeps `this` in a `DerivedThis` slot becomes `ReturnDerived`
    /// naming that slot; the VM reads it only when the frame completes, after
    /// crossed `finally` blocks.
    pub(crate) fn emit(&mut self, op: Op, operands: impl AsRef<[Operand]>, span: (u32, u32)) {
        if let Some(derived) = self.derived_this
            && matches!(op, Op::Return | Op::ReturnValue | Op::ReturnUndefined)
        {
            let value = match operands.as_ref().first() {
                Some(Operand::Register(reg)) => *reg,
                _ => {
                    let reg = self.alloc_scratch();
                    self.push_raw(Op::LoadUndefined, &[Operand::Register(reg)], span);
                    reg
                }
            };
            let coord = ContextCoord {
                depth: 0,
                slot: derived.slot,
            }
            .to_imm32();
            self.push_raw(
                Op::ReturnDerived,
                &[
                    Operand::Register(value),
                    Operand::Register(derived.ctx),
                    Operand::Imm32(coord),
                ],
                span,
            );
            return;
        }
        self.push_raw(op, operands.as_ref(), span);
    }

    fn push_raw(&mut self, op: Op, operands: &[Operand], span: (u32, u32)) {
        let pc = self.next_pc();
        self.code.push(op, operands);
        self.spans.push(SpanEntry { pc, span });
    }

    /// Emit `op` whose operand `ctx_index` names the context in `ctx`.
    /// A closure-context operand is recorded for finalization patching.
    pub(crate) fn emit_ctx(
        &mut self,
        op: Op,
        mut operands: Vec<Operand>,
        ctx_index: usize,
        ctx: CtxReg,
        span: (u32, u32),
    ) {
        match ctx {
            CtxReg::Reg(reg) => operands[ctx_index] = Operand::Register(reg),
            CtxReg::Closure if self.closure_context_empty => {
                let reg = self.alloc_scratch();
                self.emit(Op::LoadUndefined, [Operand::Register(reg)], span);
                operands[ctx_index] = Operand::Register(reg);
            }
            CtxReg::Closure => {
                operands[ctx_index] = Operand::Register(0);
                let pc = self.next_pc();
                self.closure_ctx_patches.push((pc, ctx_index));
            }
        }
        self.emit(op, operands, span);
    }

    /// Logical PC assigned to the next emitted instruction.
    pub(crate) fn next_pc(&self) -> u32 {
        self.code.next_pc()
    }

    /// UpdateEmpty(…, undefined) — a composite statement resets the
    /// program completion register on entry.
    pub(crate) fn emit_completion_reset(&mut self, span: (u32, u32)) {
        if self.completion_suppressed {
            return;
        }
        if let Some(reg) = self.completion_reg {
            self.emit(Op::LoadUndefined, [Operand::Register(reg)], span);
        }
    }

    /// Whether a statement's completion value can still be observed.
    pub(crate) fn completion_tracking(&self) -> bool {
        self.completion_reg.is_some() && !self.completion_suppressed
    }

    /// Reserve the running completion register (`V`) a statement form threads
    /// its body completions through, initialized to `undefined`.
    pub(crate) fn alloc_completion_reg(&mut self, span: (u32, u32)) -> Option<u16> {
        if !self.completion_tracking() {
            return None;
        }
        let reg = self.alloc_scratch();
        self.emit(Op::LoadUndefined, [Operand::Register(reg)], span);
        Some(reg)
    }

    /// Record one non-empty body completion into a form's running `V`.
    pub(crate) fn store_completion(
        &mut self,
        completion: Option<u16>,
        value_reg: u16,
        span: (u32, u32),
    ) {
        if let Some(reg) = completion
            && reg != value_reg
        {
            self.emit(
                Op::StoreLocal,
                [Operand::Register(value_reg), Operand::Imm32(reg as i32)],
                span,
            );
        }
    }

    /// Statement-list `V` threading.
    pub(crate) fn emit_completion_value(&mut self, value_reg: u16, span: (u32, u32)) {
        if self.completion_suppressed {
            return;
        }
        if let Some(reg) = self.completion_reg
            && reg != value_reg
        {
            self.emit(
                Op::StoreLocal,
                [Operand::Register(value_reg), Operand::Imm32(reg as i32)],
                span,
            );
        }
    }

    /// Emit `Op::StoreProperty obj_reg, name_const, src_reg, scratch`.
    pub(crate) fn emit_store_property(
        &mut self,
        obj_reg: u16,
        name: &str,
        src: u16,
        span: (u32, u32),
    ) {
        let name_const = self.intern_string_constant(name);
        let scratch = self.alloc_scratch();
        let op = self.store_property_op();
        self.emit(
            op,
            vec![
                Operand::Register(obj_reg),
                Operand::ConstIndex(name_const),
                Operand::Register(src),
                Operand::Register(scratch),
            ],
            span,
        );
    }

    /// §15.7.1 — the property-store opcode for PutValue emission at
    /// the current compile point.
    pub(crate) fn store_property_op(&self) -> Op {
        if self.strict_class_parts {
            Op::StorePropertyStrict
        } else {
            Op::StoreProperty
        }
    }

    /// Element counterpart of [`Self::store_property_op`].
    pub(crate) fn store_element_op(&self) -> Op {
        if self.strict_class_parts {
            Op::StoreElementStrict
        } else {
            Op::StoreElement
        }
    }

    /// Emit `Op::StoreElement obj_reg, key_reg, src_reg`.
    pub(crate) fn emit_store_element(
        &mut self,
        obj_reg: u16,
        key_reg: u16,
        src: u16,
        span: (u32, u32),
    ) {
        let op = self.store_element_op();
        self.emit(
            op,
            vec![
                Operand::Register(obj_reg),
                Operand::Register(key_reg),
                Operand::Register(src),
            ],
            span,
        );
    }

    /// Emit `Op::LoadProperty dst, obj_reg, name_const`.
    pub(crate) fn emit_load_property(
        &mut self,
        dst: u16,
        obj_reg: u16,
        name: &str,
        span: (u32, u32),
    ) {
        let name_const = self.intern_string_constant(name);
        self.emit(
            Op::LoadProperty,
            [
                Operand::Register(dst),
                Operand::Register(obj_reg),
                Operand::ConstIndex(name_const),
            ],
            span,
        );
    }

    /// Finalize the body: route every closure-context operand to one fresh
    /// register filled by a `LoadClosureContext` prepended at pc 0, and
    /// hand back the code, spans, and scope table.
    ///
    /// Branch offsets are PC-relative, so the prepended instruction shifts
    /// every target uniformly; source spans, hint sites, and handler PCs
    /// shift by one.
    pub(crate) fn finish_code(&mut self, entry_span: (u32, u32)) -> FinishedCode {
        let mut scratch = self.scratch_window();
        let mut code = std::mem::take(&mut self.code);
        let mut spans = std::mem::take(&mut self.spans);
        let mut number_hint_sites = std::mem::take(&mut self.number_hint_sites);
        let mut class_hint_sites = std::mem::take(&mut self.class_hint_sites);
        let mut handlers = std::mem::take(&mut self.handlers);
        let uses_closure_context = !self.closure_ctx_patches.is_empty();
        if uses_closure_context {
            let reg = scratch;
            match scratch.checked_add(1) {
                Some(next) => scratch = next,
                None => self.register_overflow = true,
            }
            for &(pc, index) in &self.closure_ctx_patches {
                assert!(
                    code.set_operand(pc, index, Operand::Register(reg)),
                    "closure-context operand is a register"
                );
            }
            let mut rebuilt = FunctionCodeBuilder::new();
            rebuilt.push(Op::LoadClosureContext, &[Operand::Register(reg)]);
            for pc in 0..code.len() as u32 {
                let op = code.op(pc).expect("compiler instruction");
                let operands: Vec<Operand> = (0..code.operand_count(pc).expect("operand count"))
                    .map(|index| code.operand(pc, index).expect("compiler operand"))
                    .collect();
                rebuilt.push(op, &operands);
            }
            code = rebuilt;
            for span in &mut spans {
                span.pc += 1;
            }
            spans.insert(
                0,
                SpanEntry {
                    pc: 0,
                    span: entry_span,
                },
            );
            for pc in &mut number_hint_sites {
                *pc += 1;
            }
            for (pc, _) in &mut class_hint_sites {
                *pc += 1;
            }
            for handler in &mut handlers {
                handler.start += 1;
                handler.end += 1;
                handler.target += 1;
            }
            self.closure_ctx_patches.clear();
        }
        FinishedCode {
            code: code.finish(),
            spans,
            scratch,
            scopes: std::mem::take(&mut self.scope_descriptors),
            uses_closure_context,
            number_hint_sites,
            class_hint_sites,
            handlers,
        }
    }
}
