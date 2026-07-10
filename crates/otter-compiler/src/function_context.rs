//! Per-function bytecode emission state and helper methods.
//!
//! # Contents
//! - instruction emission
//! - constant interning
//! - scope and capture helpers
//! - jump patching
//!
//! # Invariants
//! - Instruction spans are emitted alongside bytecode positions.
//!
//! # See also
//! - `compiler` for the context stack

use crate::*;

/// Per-function compilation context.
#[derive(Debug)]
pub(crate) struct FunctionContext {
    pub(crate) module: Rc<RefCell<ModuleBuilder>>,
    pub(crate) code: Vec<Instruction>,
    pub(crate) spans: Vec<SpanEntry>,
    pub(crate) next_pc: u32,
    pub(crate) scratch: u16,
    /// Stack of lexical scopes. Index 0 is the function-body
    /// scope.
    pub(crate) scopes: Vec<Scope>,
    /// ECMAScript strictness for the function currently being
    /// lowered. This is compile-time metadata stored on the
    /// resulting bytecode function and also drives early errors.
    pub(crate) is_strict: bool,
    /// `true` when this context lowers an arrow function. Arrows have
    /// no own `arguments` binding, which changes the
    /// EvalDeclarationInstantiation `var arguments` early error
    /// (§19.2.1.3) for direct eval call sites inside the body.
    pub(crate) is_arrow: bool,
    /// `true` when `super.x` in this context resolves its
    /// [[HomeObject]] through the class STATICS side
    /// (`__class_static_home`): static methods / accessors, static
    /// blocks, and static field initializers. Arrows inherit the
    /// flag lexically from their enclosing context.
    pub(crate) super_home_static: bool,
    /// `true` while formal-parameter defaults of this function are
    /// being lowered. A direct eval in that window var-declaring
    /// `arguments` is an early SyntaxError when [`Self::binds_arguments`]
    /// holds (§19.2.1.3); after parameter instantiation the binding is
    /// initialized and the same eval body is legal.
    pub(crate) in_param_init: bool,
    /// `true` when this function will have an `arguments` binding in
    /// its variable environment: every non-arrow function, and arrows
    /// only when a parameter / body `var` / body lexical / body
    /// function declaration introduces the name.
    pub(crate) binds_arguments: bool,
    /// Canonical source URL inherited by nested functions. Dynamic
    /// import uses this as the referrer when it runs inside a
    /// function body rather than at top level.
    pub(crate) module_url: String,
    pub(crate) is_async_generator: bool,
    /// Stack of enclosing loops; the innermost is on top.
    pub(crate) loops: Vec<LoopFrame>,
    /// Count of active `try` handlers (`EnterTry` not yet `LeaveTry`'d)
    /// at the current compile point. Mirrors the runtime frame
    /// handler-stack depth so `break`/`continue` can pass a `floor` to
    /// [`otter_bytecode::Op::JumpViaFinally`].
    pub(crate) active_handlers: u32,
    /// Subset of [`Self::active_handlers`] that carry a `finally`
    /// block. `break`/`continue` only need the finally-routing opcode
    /// when this exceeds the target loop's recorded floor.
    pub(crate) active_finally: u32,
    /// Label deposited by the immediately-enclosing
    /// `LabeledStatement` waiting to be consumed by the next pushed
    /// loop / switch frame. See [`compile_labeled_statement`].
    pub(crate) pending_label: Option<String>,
    /// Names that the entry-point pre-pass already compiled +
    /// stored as hoisted function declarations. The
    /// `Statement::FunctionDeclaration` arm checks this set and
    /// skips the source-position emission so the function isn't
    /// recompiled and its closure isn't re-stored.
    /// <https://tc39.es/ecma262/#sec-functiondeclarationinstantiation>
    pub(crate) hoisted_function_names: HashSet<String>,
    /// §B.3.3 — block-level function names receiving the sloppy-mode
    /// var-scope extension, mapped to the variable-scope storage the
    /// declaration's source position syncs into. The `bool` marks
    /// global-script bindings that also mirror via `DefineGlobalVar`.
    pub(crate) annex_b_var_storages:
        std::collections::HashMap<String, (Option<crate::scope::BindingStorage>, bool)>,
    /// `true` when an anonymous `export default function/function*` was
    /// already hoisted (compiled + mirrored to `module_env.default`) at
    /// instantiation, so its source-position arm must be a no-op.
    pub(crate) default_function_hoisted: bool,
    /// Names of this function's own bindings that some nested
    /// function references — populated by
    /// [`capture::analyze_function`] before code gen starts. Each
    /// such binding is allocated as an
    /// [`UpvalueCell`](otter_vm::UpvalueCell) instead of a register.
    pub(crate) captured_names: HashSet<String>,
    /// Simple formal names that must live in own-upvalue cells so a
    /// sloppy mapped arguments object can alias them without exposing
    /// frame registers outside the VM.
    pub(crate) mapped_argument_names: HashSet<String>,
    /// Pre-assigned own-upvalue slots for names known before codegen
    /// starts. This keeps later parent-capture slots stable when
    /// parameter default expressions capture outer bindings before
    /// body `var` declarations are lowered.
    pub(crate) reserved_own_upvalues: HashMap<String, u16>,
    /// Number of own-upvalue cells allocated so far. The first
    /// `own_upvalue_count` slots in `frame.upvalues` belong to this
    /// function's own captured bindings.
    pub(crate) own_upvalue_count: u16,
    /// One entry per capture from the enclosing function. Each
    /// value is an absolute index into the **enclosing** frame's
    /// `upvalues` array — used as the source operand of
    /// `MakeClosure` when the parent emits the closure value.
    pub(crate) parent_captures: Vec<u32>,
    /// Map from captured-name → upvalue index in **this** function's
    /// `frame.upvalues`. Captures live at
    /// `own_upvalue_count..own_upvalue_count + parent_captures.len()`.
    pub(crate) captured_uv: HashMap<String, u16>,
    /// `Some` when this context is the top-level `<module-init>`
    /// of an ES-module fragment. Drives the lowering of
    /// `import` / `export` declarations + `import.meta` references
    /// against captured `module_env` / `import_meta` upvalues.
    /// Inner functions inherit module-mode lookups via the
    /// existing capture walk — they never set this themselves.
    pub(crate) module_state: Option<ModuleState>,
    /// Synthetic object-environment bindings introduced by sloppy
    /// `with` statements that enclose the code currently being
    /// lowered. Entries are binding names whose values are captured
    /// `JsObject` references.
    pub(crate) active_with_envs: Vec<crate::with_statement::WithEnv>,
    /// Set when `alloc_scratch` exhausted the u16 register window;
    /// surfaced as a CompileError when the function is finalized.
    pub(crate) register_overflow: bool,
    /// High-water mark of `alloc_scratch` — the real window size
    /// when call sites recycle registers by rolling `scratch` back.
    pub(crate) scratch_peak: u16,
    /// Monotonic suffix for synthetic `with` bindings in this
    /// function.
    pub(crate) next_with_env_id: u32,
    /// `true` when this function body contains a direct-eval call
    /// site. Every function-scope binding is promoted to an
    /// own-upvalue cell and the name → cell map is recorded on the
    /// emitted [`Function::direct_eval_bindings`] so `Op::Eval` can
    /// hand the eval body its caller variable environment
    /// (§19.2.1.3 EvalDeclarationInstantiation).
    pub(crate) contains_direct_eval: bool,
    /// §8.4 / §14 — the script / eval `<main>` completion-value
    /// register (spec `V`). Expression statements store into it as
    /// they evaluate; composite statements reset it to `undefined`
    /// on entry (their completion is never *empty*, per UpdateEmpty
    /// with `undefined`). `None` inside ordinary function bodies —
    /// only program completion is observable (via `eval` /
    /// `evalScript` return values).
    pub(crate) completion_reg: Option<u16>,
    /// `true` while lowering a `finally` block body: a normal
    /// finalizer's completion value is discarded (§14.15.3 step 4),
    /// so its statements must not touch the completion register.
    pub(crate) completion_suppressed: bool,
    /// Number of `finally` block BODIES currently being lowered. A
    /// `break`/`continue` whose target lies outside `n` of them must
    /// discard the `n` completions those finallys parked
    /// ([`otter_bytecode::Op::PopParkedFinally`]).
    pub(crate) finally_body_depth: u32,
}

impl FunctionContext {
    pub(crate) fn new(module: Rc<RefCell<ModuleBuilder>>) -> Self {
        Self {
            module,
            code: Vec::new(),
            spans: Vec::new(),
            next_pc: 0,
            scratch: 0,
            scopes: Vec::new(),
            is_strict: false,
            is_arrow: false,
            super_home_static: false,
            in_param_init: false,
            binds_arguments: false,
            module_url: String::new(),
            is_async_generator: false,
            loops: Vec::new(),
            active_handlers: 0,
            active_finally: 0,
            pending_label: None,
            hoisted_function_names: HashSet::new(),
            annex_b_var_storages: std::collections::HashMap::new(),
            default_function_hoisted: false,
            captured_names: HashSet::new(),
            mapped_argument_names: HashSet::new(),
            reserved_own_upvalues: HashMap::new(),
            own_upvalue_count: 0,
            parent_captures: Vec::new(),
            captured_uv: HashMap::new(),
            module_state: None,
            active_with_envs: Vec::new(),
            register_overflow: false,
            scratch_peak: 0,
            next_with_env_id: 0,
            contains_direct_eval: false,
            completion_reg: None,
            completion_suppressed: false,
            finally_body_depth: 0,
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

    pub(crate) fn reserve_known_own_upvalues(&mut self) {
        if !self.reserved_own_upvalues.is_empty() {
            return;
        }
        let mut names: Vec<String> = self
            .captured_names
            .union(&self.mapped_argument_names)
            .cloned()
            .collect();
        names.sort();
        for name in names {
            let idx = self.own_upvalue_count;
            self.own_upvalue_count = idx.checked_add(1).expect("own_upvalue_count overflow");
            self.reserved_own_upvalues.insert(name, idx);
        }
    }

    /// Check `name` against this function's `captured_names` set
    /// (computed by the pre-pass) and, when present, allocate a
    /// fresh own-upvalue index for it. Returns the assigned index
    /// or `None` if the name is not captured (use a register
    /// instead).
    pub(crate) fn allocate_own_upvalue(&mut self, name: &str) -> Option<u16> {
        if !self.captured_names.contains(name) && !self.mapped_argument_names.contains(name) {
            return None;
        }
        if let Some(&idx) = self.reserved_own_upvalues.get(name) {
            return Some(idx);
        }
        let idx = self.own_upvalue_count;
        self.own_upvalue_count = idx.checked_add(1).expect("own_upvalue_count overflow");
        Some(idx)
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
    /// low register range instead of stacking new ones. That overlap is
    /// what shrinks the frame's `scratch_peak` (and hence its register
    /// count), not just the bytecode's register numbering.
    ///
    /// Sound because (a) expression lowering never declares a persistent
    /// binding — those are statement-level and always sit below `mark` —
    /// so no live value beneath the result is clobbered, and (b) every
    /// result opcode reads all of its source operands before writing its
    /// destination, so a destination that aliases a just-freed operand
    /// register is read-before-write safe.
    pub(crate) fn reset_scratch(&mut self, mark: u16) {
        debug_assert!(
            mark <= self.scratch,
            "reset_scratch above current watermark"
        );
        self.scratch = mark;
    }

    /// Final register-window size: the high-water mark survives
    /// scratch recycling (`cx.scratch = mark` rollbacks).
    pub(crate) fn scratch_window(&self) -> u16 {
        self.scratch.max(self.scratch_peak)
    }

    /// Push `frame` onto the loop stack, consuming any pending
    /// `LabeledStatement` label so `break label;` / `continue label;`
    /// inside the body resolves to this frame.
    pub(crate) fn push_loop_frame(&mut self, mut frame: LoopFrame) {
        // A synthetic labeled-block frame arrives with its label
        // already set — only loop/switch frames consume the pending
        // label stashed by `compile_labeled_statement`.
        if frame.label.is_none() {
            frame.label = self.pending_label.take();
        }
        frame.handler_floor = self.active_handlers;
        frame.finally_floor = self.active_finally;
        frame.finally_body_floor = self.finally_body_depth;
        self.loops.push(frame);
    }

    pub(crate) fn enter_scope(&mut self) {
        self.scopes.push(Scope::default());
    }

    pub(crate) fn exit_scope(&mut self) {
        self.scopes.pop();
    }

    /// Declare a synthetic binding whose storage is **always** an
    /// own-upvalue cell, regardless of whether the capture pre-pass
    /// flagged the name. Used by class lowering to set up
    /// `__class_home` and `__class_super` slots that inner methods
    /// resolve through the standard `resolve_capture` walk.
    pub(crate) fn declare_captured_binding(
        &mut self,
        name: &str,
        is_const: bool,
        span: (u32, u32),
    ) -> Result<BindingStorage, CompileError> {
        if self
            .scopes
            .last()
            .expect("declare_captured_binding called outside any scope")
            .bindings
            .contains_key(name)
        {
            return Err(CompileError::Unsupported {
                node: format!("redeclaration of `{name}` in same scope"),
                span,
            });
        }
        let idx = self.own_upvalue_count;
        self.own_upvalue_count = idx.checked_add(1).expect("own_upvalue_count overflow");
        let storage = BindingStorage::Upvalue { idx };
        let scope = self
            .scopes
            .last_mut()
            .expect("declare_captured_binding called outside any scope");
        scope.bindings.insert(
            name.to_string(),
            BindingInfo {
                storage,
                is_const,
                initialized: false,
                fn_self_name: false,
            },
        );
        Ok(storage)
    }

    pub(crate) fn declare_binding(
        &mut self,
        name: &str,
        is_const: bool,
        span: (u32, u32),
    ) -> Result<BindingStorage, CompileError> {
        self.declare_binding_with_capture(name, is_const, span, true)
    }

    pub(crate) fn declare_binding_with_capture(
        &mut self,
        name: &str,
        is_const: bool,
        span: (u32, u32),
        allow_capture: bool,
    ) -> Result<BindingStorage, CompileError> {
        if self
            .scopes
            .last()
            .expect("declare_binding called outside any scope")
            .bindings
            .contains_key(name)
        {
            return Err(CompileError::Unsupported {
                node: format!("redeclaration of `{name}` in same scope"),
                span,
            });
        }
        let storage = if allow_capture && let Some(idx) = self.allocate_own_upvalue(name) {
            BindingStorage::Upvalue { idx }
        } else {
            let reg = self.scratch;
            self.scratch = self.scratch.checked_add(1).expect("register overflow");
            BindingStorage::Register { reg }
        };
        let scope = self
            .scopes
            .last_mut()
            .expect("declare_binding called outside any scope");
        scope.bindings.insert(
            name.to_string(),
            BindingInfo {
                storage,
                is_const,
                initialized: false,
                fn_self_name: false,
            },
        );
        Ok(storage)
    }

    pub(crate) fn lookup_binding(&self, name: &str) -> Option<BindingInfo> {
        for scope in self.scopes.iter().rev() {
            if let Some(info) = scope.bindings.get(name) {
                return Some(*info);
            }
        }
        None
    }

    /// As [`Self::lookup_binding`], also reporting the 0-based scope
    /// index the name resolved in (0 = the function/script top scope
    /// where hoisted `var`s live). Lets `var` initializers detect a
    /// shadowing inner binding (a catch parameter, §B.3.5) whose store
    /// must not mirror to the global / module export.
    pub(crate) fn lookup_binding_with_depth(&self, name: &str) -> Option<(BindingInfo, usize)> {
        for (idx, scope) in self.scopes.iter().enumerate().rev() {
            if let Some(info) = scope.bindings.get(name) {
                return Some((*info, idx));
            }
        }
        None
    }

    /// Look up `name` only in the *innermost* scope. Used by the
    /// `let` / `const` arm to detect bindings the lexical pre-pass
    /// already created at the function / script / module top level.
    pub(crate) fn lookup_in_current_scope(&self, name: &str) -> Option<BindingInfo> {
        self.scopes
            .last()
            .and_then(|scope| scope.bindings.get(name).copied())
    }

    /// Flip a binding's `initialized` flag to `true` once we've
    /// emitted its initializer's store. The compiler is intentionally
    /// conservative: we never flip back to `false` and we never
    /// "merge" branch states — task 14 ships the simple definite-
    /// assignment rule and leaves branch-aware refinement for a
    /// future slice.
    /// Flag `name`'s innermost binding as a §10.2.11 named
    /// function expression self-name (immutable; sloppy writes
    /// silently drop, strict writes throw TypeError).
    pub(crate) fn mark_fn_self_name(&mut self, name: &str) {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(info) = scope.bindings.get_mut(name) {
                info.fn_self_name = true;
                return;
            }
        }
    }

    pub(crate) fn mark_initialized(&mut self, name: &str) {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(info) = scope.bindings.get_mut(name) {
                info.initialized = true;
                return;
            }
        }
    }

    /// Emit an [`Op::EnterTry`] with placeholder catch / finally
    /// offsets and an exception register. Returns the instruction
    /// pc so the caller can patch the targeted offset to the
    /// emitted catch / finally landing.
    ///
    /// `catch_offset` and `finally_offset` are the **initial**
    /// values stored in the operand list — typically a real
    /// `0` placeholder for whichever clause needs patching, and
    /// [`otter_vm::NO_HANDLER_OFFSET`] for the absent clause.
    pub(crate) fn emit_enter_try(
        &mut self,
        catch_offset: i32,
        finally_offset: i32,
        exc_reg: u16,
        span: (u32, u32),
    ) -> u32 {
        let pc = self.next_pc;
        self.code.push(Instruction {
            pc,
            op: Op::EnterTry,
            operands: [
                Operand::Imm32(catch_offset),
                Operand::Imm32(finally_offset),
                Operand::Register(exc_reg),
            ]
            .into(),
        });
        self.spans.push(SpanEntry { pc, span });
        self.next_pc += 1;
        pc
    }

    /// Patch a previously emitted [`Op::EnterTry`] so that one of
    /// its offsets targets the **current** `next_pc`. Pass `true`
    /// for `is_catch` to patch the catch offset, `false` to patch
    /// the finally offset. The non-targeted offset is left
    /// untouched (kept as the `NO_HANDLER_OFFSET` sentinel the
    /// initial emit installed).
    pub(crate) fn patch_enter_try_offset(&mut self, enter_pc: u32, is_catch: bool) {
        let target = self.next_pc;
        let offset = target as i64 - (enter_pc as i64 + 1);
        let offset = i32::try_from(offset).expect("EnterTry offset out of i32 range");
        let instr = self
            .code
            .iter_mut()
            .find(|i| i.pc == enter_pc)
            .expect("patch target missing");
        debug_assert!(matches!(instr.op, Op::EnterTry));
        let slot_idx = if is_catch { 0 } else { 1 };
        match instr.operands.get_mut(slot_idx) {
            Some(Operand::Imm32(slot)) => *slot = offset,
            _ => panic!("EnterTry operand at index {slot_idx} not Imm32"),
        }
    }

    /// Emit a placeholder branch and return its instruction index
    /// so a later [`Self::patch_branch`] can fill in the offset.
    pub(crate) fn emit_branch_placeholder(
        &mut self,
        op: Op,
        cond_reg: Option<u16>,
        span: (u32, u32),
    ) -> u32 {
        let pc = self.next_pc;
        let operands = if let Some(reg) = cond_reg {
            [Operand::Imm32(0), Operand::Register(reg)].into()
        } else {
            [Operand::Imm32(0)].into()
        };
        self.code.push(Instruction { pc, op, operands });
        self.spans.push(SpanEntry { pc, span });
        self.next_pc += 1;
        pc
    }

    /// Patch a previously emitted branch so it targets the
    /// **current** `next_pc`.
    pub(crate) fn patch_branch_to_here(&mut self, branch_pc: u32) {
        let target = self.next_pc;
        self.patch_branch(branch_pc, target);
    }

    /// Patch a previously emitted branch to point at `target_pc`.
    pub(crate) fn patch_branch(&mut self, branch_pc: u32, target_pc: u32) {
        let offset = target_pc as i64 - (branch_pc as i64 + 1);
        let offset = i32::try_from(offset).expect("branch offset out of i32 range");
        let instr = self
            .code
            .iter_mut()
            .find(|i| i.pc == branch_pc)
            .expect("patch target missing");
        if let Some(Operand::Imm32(slot)) = instr.operands.first_mut() {
            *slot = offset;
        } else {
            panic!("patch target operand not Imm32");
        }
    }

    pub(crate) fn intern_string_constant(&mut self, value: &str) -> u32 {
        let utf16: Vec<u16> = value.encode_utf16().collect();
        self.intern_utf16_string_constant(utf16)
    }

    /// Intern a pre-built WTF-16 unit vector. Used for string
    /// literals that carry lone surrogates: oxc encodes those via
    /// the §11.8.4 [`StringLiteral`](https://tc39.es/ecma262/#sec-literals-string-literals)
    /// lossy scheme (`\u{FFFD}XXXX` per lone surrogate, `\u{FFFD}fffd`
    /// for a literal U+FFFD), so the compiler decodes it into the
    /// original code-unit sequence before interning.
    pub(crate) fn intern_utf16_string_constant(&mut self, utf16: Vec<u16>) -> u32 {
        let mut module = self.module.borrow_mut();
        for (i, c) in module.constants.iter().enumerate() {
            if let Constant::String { utf16: existing } = c
                && existing == &utf16
            {
                return i as u32;
            }
        }
        module.constants.push(Constant::String { utf16 });
        (module.constants.len() - 1) as u32
    }

    pub(crate) fn intern_number_constant(&mut self, value: f64) -> u32 {
        let bits = value.to_bits();
        let mut module = self.module.borrow_mut();
        for (i, c) in module.constants.iter().enumerate() {
            if let Constant::Number { bits: existing } = c
                && *existing == bits
            {
                return i as u32;
            }
        }
        module.constants.push(Constant::Number { bits });
        (module.constants.len() - 1) as u32
    }

    pub(crate) fn intern_bigint_constant(&mut self, decimal: &str) -> u32 {
        let mut module = self.module.borrow_mut();
        for (i, c) in module.constants.iter().enumerate() {
            if let Constant::BigInt { decimal: existing } = c
                && existing == decimal
            {
                return i as u32;
            }
        }
        module.constants.push(Constant::BigInt {
            decimal: decimal.to_string(),
        });
        (module.constants.len() - 1) as u32
    }

    pub(crate) fn intern_regexp_constant(&mut self, pattern_utf16: &[u16], flags: &str) -> u32 {
        let mut module = self.module.borrow_mut();
        for (i, c) in module.constants.iter().enumerate() {
            if let Constant::RegExp {
                pattern_utf16: existing_pat,
                flags: existing_flags,
            } = c
                && existing_pat == pattern_utf16
                && existing_flags == flags
            {
                return i as u32;
            }
        }
        module.constants.push(Constant::RegExp {
            pattern_utf16: pattern_utf16.to_vec(),
            flags: flags.to_string(),
        });
        (module.constants.len() - 1) as u32
    }

    pub(crate) fn intern_function_id(&mut self, function_id: u32) -> u32 {
        let mut module = self.module.borrow_mut();
        for (i, c) in module.constants.iter().enumerate() {
            if let Constant::FunctionId { index } = c
                && *index == function_id
            {
                return i as u32;
            }
        }
        module
            .constants
            .push(Constant::FunctionId { index: function_id });
        (module.constants.len() - 1) as u32
    }

    pub(crate) fn emit(&mut self, op: Op, operands: impl Into<OperandList>, span: (u32, u32)) {
        let pc = self.next_pc;
        self.code.push(Instruction {
            pc,
            op,
            operands: operands.into(),
        });
        self.spans.push(SpanEntry { pc, span });
        self.next_pc += 1;
    }

    /// UpdateEmpty(…, undefined) — a composite statement (`if`,
    /// loops, `switch`, `try`, `with`, labelled) resets the program
    /// completion register on entry: its completion is `undefined`
    /// unless an inner statement produces a value.
    pub(crate) fn emit_completion_reset(&mut self, span: (u32, u32)) {
        if self.completion_suppressed {
            return;
        }
        if let Some(reg) = self.completion_reg {
            self.emit(Op::LoadUndefined, [Operand::Register(reg)], span);
        }
    }

    /// Statement-list `V` threading — store a non-empty statement
    /// completion value into the program completion register.
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

    /// Emit the appropriate "load this binding into `dst`" op pair
    /// for the binding's storage kind.
    pub(crate) fn emit_load_storage(
        &mut self,
        dst: u16,
        storage: BindingStorage,
        span: (u32, u32),
    ) {
        match storage {
            BindingStorage::Register { reg } => self.emit(
                Op::LoadLocal,
                [Operand::Register(dst), Operand::Imm32(reg as i32)],
                span,
            ),
            BindingStorage::Upvalue { idx } => self.emit(
                Op::LoadUpvalue,
                [Operand::Register(dst), Operand::Imm32(idx as i32)],
                span,
            ),
        }
    }

    /// Mirror `value_reg` through to `module_env.default`. Used by
    /// `export default function f(){}` from the hoist pass: the
    /// default export entry was registered by the module pre-pass
    /// so the closure must land on `module_env.default` even when
    /// no source-position store ever runs (the source-position arm
    /// becomes a no-op for hoisted names).
    ///
    /// Spec: <https://tc39.es/ecma262/#sec-exports-runtime-semantics-evaluation>
    pub(crate) fn emit_module_export_default_mirror(&mut self, value_reg: u16, span: (u32, u32)) {
        let env_uv = match &self.module_state {
            Some(state) => state.module_env_uv,
            None => return,
        };
        let env_reg = self.alloc_scratch();
        self.emit(
            Op::LoadUpvalue,
            [Operand::Register(env_reg), Operand::Imm32(env_uv as i32)],
            span,
        );
        self.emit_store_property(env_reg, "default", value_reg, span);
    }

    /// Emit `Op::StoreProperty obj_reg, name_const, src_reg, scratch`.
    /// Used by the module-mode lowering to mirror writes through
    /// to `module_env` for exported bindings, and by the export
    /// declaration arms. The `scratch` slot is reserved for
    /// accessor-setter dispatch per [`Op::StoreProperty`]'s contract.
    pub(crate) fn emit_store_property(
        &mut self,
        obj_reg: u16,
        name: &str,
        src: u16,
        span: (u32, u32),
    ) {
        let name_const = self.intern_string_constant(name);
        let scratch = self.alloc_scratch();
        self.emit(
            Op::StoreProperty,
            vec![
                Operand::Register(obj_reg),
                Operand::ConstIndex(name_const),
                Operand::Register(src),
                Operand::Register(scratch),
            ],
            span,
        );
    }

    /// Emit `Op::StoreElement obj_reg, key_reg, src_reg, scratch`.
    /// The scratch slot is reserved for computed-property accessor
    /// setter dispatch and mirrors `Op::StoreProperty`.
    pub(crate) fn emit_store_element(
        &mut self,
        obj_reg: u16,
        key_reg: u16,
        src: u16,
        span: (u32, u32),
    ) {
        let scratch = self.alloc_scratch();
        self.emit(
            Op::StoreElement,
            vec![
                Operand::Register(obj_reg),
                Operand::Register(key_reg),
                Operand::Register(src),
                Operand::Register(scratch),
            ],
            span,
        );
    }

    /// Emit `Op::LoadProperty dst, obj_reg, name_const`. Used by
    /// the module-mode lowering for imported-name reads
    /// (`LoadProperty import_record, "name"`) and `import.meta.url`.
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

    /// Emit the "write `src` into this binding" op pair for the
    /// storage kind. Used for *binding initialization* (declarations,
    /// loop-head per-iteration bindings) — it clears any TDZ hole.
    pub(crate) fn emit_store_storage(
        &mut self,
        src: u16,
        storage: BindingStorage,
        span: (u32, u32),
    ) {
        match storage {
            BindingStorage::Register { reg } => self.emit(
                Op::StoreLocal,
                [Operand::Register(src), Operand::Imm32(reg as i32)],
                span,
            ),
            BindingStorage::Upvalue { idx } => self.emit(
                Op::StoreUpvalue,
                [Operand::Register(src), Operand::Imm32(idx as i32)],
                span,
            ),
        }
    }

    /// Emit a binding *assignment* store (PutValue, §6.2.4.6). Captured
    /// upvalues use [`Op::StoreUpvalueChecked`] so a write to a `let`
    /// still in its TDZ raises `ReferenceError`; a register binding has
    /// no runtime hole (the read/write TDZ is detected statically at the
    /// reference site), so a plain `StoreLocal` suffices.
    pub(crate) fn emit_assign_storage(
        &mut self,
        src: u16,
        storage: BindingStorage,
        span: (u32, u32),
    ) {
        match storage {
            BindingStorage::Register { reg } => self.emit(
                Op::StoreLocal,
                [Operand::Register(src), Operand::Imm32(reg as i32)],
                span,
            ),
            BindingStorage::Upvalue { idx } => self.emit(
                Op::StoreUpvalueChecked,
                [Operand::Register(src), Operand::Imm32(idx as i32)],
                span,
            ),
        }
    }
}

/// Compile-time marker bit for parent-capture upvalue indices.
/// `resolve_capture` cannot know the frame's FINAL own-upvalue
/// count (own captured cells may be declared after a capture
/// resolves — e.g. a nested class's synthetic cells inside a
/// constructor that already captured the outer brand), so capture
/// indices are issued in a virtual space and rewritten to
/// `own_count + position` when the function finalizes.
pub(crate) const VIRTUAL_CAPTURE_BASE: u16 = 0x8000;

/// Rewrite every virtual parent-capture index in `code` (and the
/// direct-eval binding table) to its final absolute position now
/// that the frame's own-upvalue count is known.
pub(crate) fn finalize_virtual_capture_indices(
    code: &mut [otter_bytecode::Instruction],
    direct_eval_meta: &mut [otter_bytecode::DirectEvalBinding],
    own_count: u16,
) {
    let base = VIRTUAL_CAPTURE_BASE as i32;
    let remap = |v: i32| -> i32 {
        if v >= base {
            own_count as i32 + (v - base)
        } else {
            v
        }
    };
    for instr in code.iter_mut() {
        match instr.op {
            // `FreshUpvalue` only ever targets OWN cells (for-of
            // per-iteration bindings) — never a parent capture.
            Op::LoadUpvalue | Op::StoreUpvalue | Op::StoreUpvalueChecked => {
                if let Some(slot) = instr.operands.get_mut(1)
                    && let Operand::Imm32(v) = *slot
                {
                    *slot = Operand::Imm32(remap(v));
                }
            }
            Op::LoadShadowedUpvalue => {
                if let Some(slot) = instr.operands.get_mut(2)
                    && let Operand::Imm32(v) = *slot
                {
                    *slot = Operand::Imm32(remap(v));
                }
            }
            Op::MakeClosure => {
                for slot in instr.operands.as_mut_slice().iter_mut().skip(3) {
                    if let Operand::Imm32(v) = *slot {
                        *slot = Operand::Imm32(remap(v));
                    }
                }
            }
            _ => {}
        }
    }
    for binding in direct_eval_meta.iter_mut() {
        if binding.upvalue >= VIRTUAL_CAPTURE_BASE {
            binding.upvalue = own_count + (binding.upvalue - VIRTUAL_CAPTURE_BASE);
        }
    }
}
