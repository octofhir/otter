//! Compile-time lexical scopes, binding records, and control-flow frames.
//!
//! A binding lives either in a frame register or in a slot of its scope's
//! context. A scope owns a context only once it holds a slot or anchors a
//! sloppy direct eval's extension; the context is an ordinary register the
//! function fills with `CreateContext` at the point the scope first needs it.
//!
//! # Contents
//! - [`Scope`] — one lexical scope: bindings, kind, flags, and its context.
//! - [`ScopeContext`] / [`CtxReg`] — where a scope's context lives.
//! - [`BindingInfo`] / [`BindingStorage`] — per-binding facts and storage.
//! - [`LoopFrame`] — break / continue patch lists.
//! - scope entry / exit, context creation, binding declaration, and own
//!   binding load / store on [`FunctionContext`].
//!
//! # Invariants
//! - A scope's context is created while that scope is the innermost scope of
//!   its function, so every context created later inside it chains to it.
//! - [`BindingStorage::Slot`] names the owning scope's context register; the
//!   slot index is the binding's position in that scope's descriptor.
//! - Scope kinds and slot kinds mirror the bytecode descriptor alphabet
//!   ([`otter_bytecode::ScopeKind`], [`otter_bytecode::SlotKind`]).
//!
//! # See also
//! - `function_context` for scope entry, context creation, and slot storage.
//! - `compiler` for name resolution across frames and the eval chain.

use crate::*;
use otter_bytecode::{
    ContextCoord, ScopeDescriptor, ScopeFlags, ScopeKind, SlotDescriptor, SlotKind,
};

/// Register holding a context: an ordinary frame register, or the running
/// closure's own context (read once at entry by `LoadClosureContext`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CtxReg {
    /// A frame register.
    Reg(u16),
    /// The function's closure context, materialized at entry.
    Closure,
}

/// The context a scope owns.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ScopeContext {
    /// Register holding the scope's live context.
    pub(crate) reg: CtxReg,
    /// Index into the function's scope descriptor table.
    pub(crate) descriptor: u16,
}

/// One lexical scope's binding table. The compiler keeps a stack
/// of these so block-scoped `let`/`const` shadow correctly.
#[derive(Debug)]
pub(crate) struct Scope {
    /// Map from binding name to its record.
    pub(crate) bindings: HashMap<String, BindingInfo>,
    /// Descriptor kind of the scope's context.
    pub(crate) kind: ScopeKind,
    /// Strictness, variable-environment, and extension facts.
    pub(crate) flags: ScopeFlags,
    /// The scope's context, once created.
    pub(crate) context: Option<ScopeContext>,
}

impl Scope {
    pub(crate) fn new(kind: ScopeKind, flags: ScopeFlags) -> Self {
        Self {
            bindings: HashMap::new(),
            kind,
            flags,
            context: None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct BindingInfo {
    /// Backing storage: a register, or a slot of the owning scope's context.
    pub(crate) storage: BindingStorage,
    /// `true` for `const` declarations.
    pub(crate) is_const: bool,
    /// Whether the binding has been definitely initialized at the
    /// current compile point. `let x;` and `let x = init` start at
    /// `false` and flip to `true` after the initializer's store.
    /// Reads before that emit `Op::TdzError`.
    pub(crate) initialized: bool,
    /// §10.2.11 — `true` for a named function expression's own-name
    /// binding (and a class's inner name): immutable, but assignment is a
    /// TypeError only in strict mode (sloppy writes are silently dropped
    /// after the RHS evaluates).
    pub(crate) fn_self_name: bool,
    /// Static type read off the binding's TypeScript annotation.
    /// Advisory: nothing checks it at runtime, so it may only seed
    /// speculation that a guard can undo.
    pub(crate) type_hint: TypeHint,
    /// Descriptor kind of the binding: its initial value, checks, and
    /// store behavior when it lives in a context slot.
    pub(crate) kind: SlotKind,
}

impl BindingInfo {
    pub(crate) fn new(storage: BindingStorage, kind: SlotKind) -> Self {
        Self {
            storage,
            is_const: matches!(kind, SlotKind::Const),
            initialized: false,
            fn_self_name: false,
            type_hint: TypeHint::Unknown,
            kind,
        }
    }
}

/// Where a binding lives in the running frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BindingStorage {
    /// Plain register. Read with `LoadLocal`, written with `StoreLocal`.
    Register { reg: u16 },
    /// Slot `slot` of the owning scope's context, held in `ctx`.
    Slot { ctx: CtxReg, slot: u16 },
}

impl BindingStorage {
    /// Mapped-arguments storage for a formal parameter binding.
    pub(crate) fn to_argument_storage(self) -> Option<ArgumentBindingStorage> {
        match self {
            Self::Register { reg } => Some(ArgumentBindingStorage::Register { reg }),
            Self::Slot {
                ctx: CtxReg::Reg(reg),
                slot,
            } => Some(ArgumentBindingStorage::Context { reg, slot }),
            Self::Slot {
                ctx: CtxReg::Closure,
                ..
            } => None,
        }
    }

    /// Diagnostic index carried by `Op::TdzError`.
    pub(crate) fn diagnostic_index(self) -> i32 {
        match self {
            Self::Register { reg } => i32::from(reg),
            Self::Slot { slot, .. } => i32::from(slot),
        }
    }
}

/// One pending control-flow target so `break` / `continue` can patch
/// their offsets at scope close.
///
/// Tracks both real loops (`for` / `while` / `do-while` / `for-of` /
/// `for-in`) and pseudo-loops (`switch` body — only `break` is
/// legal, `continue` skips switch frames per spec §13.10.1).
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-iteration-statements>
/// - <https://tc39.es/ecma262/#sec-switch-statement>
/// - <https://tc39.es/ecma262/#sec-labelled-statements>
#[derive(Debug, Default)]
pub(crate) struct LoopFrame {
    /// Instruction PCs where `continue` emitted a placeholder
    /// JUMP. Patched to point at the loop's continue target (the
    /// update / test).
    pub(crate) continue_patches: Vec<u32>,
    /// Instruction PCs where `break` emitted a placeholder JUMP.
    /// Patched to point at the instruction after the loop body.
    pub(crate) break_patches: Vec<u32>,
    /// Optional label attached to this frame by an enclosing
    /// `LabeledStatement`. `break label;` matches against this
    /// field walking outward; `continue label;` only matches
    /// when [`LoopFrame::is_real_loop`] is true.
    pub(crate) label: Option<String>,
    /// `true` when the frame represents an iteration statement.
    /// `false` for a `switch` body, where `continue` must skip the
    /// frame and target the enclosing loop instead.
    pub(crate) is_real_loop: bool,
    /// For a `for…of` frame, the register holding the iterator that
    /// must be closed (§7.4.9 IteratorClose) when an abrupt
    /// completion (`break` / labelled `continue` / `return`) exits the
    /// loop. `None` for every other loop / switch frame.
    pub(crate) iterator_close_reg: Option<u16>,
    /// Runtime try-handler-stack depth in effect when this frame was
    /// entered. A `break`/`continue` targeting this frame must run
    /// every `finally` pushed since (handlers above this floor).
    pub(crate) handler_floor: u32,
    /// `finally`-handler count in effect at entry; a target whose
    /// `active_finally` exceeds this needs the finally-routing
    /// `break`/`continue` opcode.
    pub(crate) finally_floor: u32,
    /// [`crate::function_context::FunctionContext::finally_body_depth`]
    /// at entry; an exit jump discards one parked completion per
    /// finally BODY it abandons.
    pub(crate) finally_body_floor: u32,
}

impl LoopFrame {
    pub(crate) fn iteration() -> Self {
        Self {
            continue_patches: Vec::new(),
            break_patches: Vec::new(),
            label: None,
            is_real_loop: true,
            iterator_close_reg: None,
            handler_floor: 0,
            finally_floor: 0,
            finally_body_floor: 0,
        }
    }

    pub(crate) fn switch_body() -> Self {
        Self {
            continue_patches: Vec::new(),
            break_patches: Vec::new(),
            label: None,
            is_real_loop: false,
            iterator_close_reg: None,
            handler_floor: 0,
            finally_floor: 0,
            finally_body_floor: 0,
        }
    }
}

impl FunctionContext {
    /// Enter a scope of `kind` with the current strictness.
    pub(crate) fn enter_scope(&mut self, kind: ScopeKind) {
        let flags = ScopeFlags {
            strict: self.is_strict,
            var_scope: false,
            has_extension: false,
        };
        self.scopes.push(Scope::new(kind, flags));
    }

    /// Enter a scope with explicit descriptor flags.
    pub(crate) fn enter_scope_with_flags(&mut self, kind: ScopeKind, flags: ScopeFlags) {
        self.scopes.push(Scope::new(kind, flags));
    }

    pub(crate) fn exit_scope(&mut self) {
        self.scopes.pop();
    }

    /// Context register of the innermost scope that owns a context, or
    /// the closure context when none does.
    pub(crate) fn innermost_ctx(&self) -> CtxReg {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.context.map(|ctx| ctx.reg))
            .unwrap_or(CtxReg::Closure)
    }

    /// Give the innermost scope a context now, emitting `CreateContext`.
    /// Idempotent.
    pub(crate) fn ensure_innermost_context(
        &mut self,
        span: (u32, u32),
    ) -> Result<ScopeContext, CompileError> {
        let index = self
            .scopes
            .len()
            .checked_sub(1)
            .expect("context requested outside any scope");
        if let Some(ctx) = self.scopes[index].context {
            return Ok(ctx);
        }
        let descriptor =
            u16::try_from(self.scope_descriptors.len()).map_err(|_| CompileError::Unsupported {
                node: "function creates more than 65536 scope contexts".to_string(),
                span,
            })?;
        let scope = &self.scopes[index];
        self.scope_descriptors.push(ScopeDescriptor {
            kind: scope.kind,
            flags: scope.flags,
            slots: Vec::new(),
        });
        let parent = self.scopes[..index]
            .iter()
            .rev()
            .find_map(|scope| scope.context.map(|ctx| ctx.reg))
            .unwrap_or(CtxReg::Closure);
        let reg = self.alloc_scratch();
        self.emit_ctx(
            Op::CreateContext,
            vec![
                Operand::Register(reg),
                Operand::Register(0),
                Operand::Imm32(i32::from(descriptor)),
            ],
            1,
            parent,
            span,
        );
        let ctx = ScopeContext {
            reg: CtxReg::Reg(reg),
            descriptor,
        };
        self.scopes[index].context = Some(ctx);
        Ok(ctx)
    }

    /// Install the module scope: scope 0 owns descriptor 0, whose context
    /// the runtime allocates and hands to `<module-init>` as its closure
    /// context. No `CreateContext` is emitted for it.
    pub(crate) fn install_runtime_created_scope(&mut self) {
        debug_assert!(self.scope_descriptors.is_empty() && self.scopes.len() == 1);
        let scope = &mut self.scopes[0];
        self.scope_descriptors.push(ScopeDescriptor {
            kind: scope.kind,
            flags: scope.flags,
            slots: Vec::new(),
        });
        scope.context = Some(ScopeContext {
            reg: CtxReg::Closure,
            descriptor: 0,
        });
    }

    /// Append a slot for `name` to the innermost scope's context,
    /// creating the context first when needed.
    fn allocate_slot(
        &mut self,
        name: &str,
        kind: SlotKind,
        span: (u32, u32),
    ) -> Result<(CtxReg, u16), CompileError> {
        let ctx = self.ensure_innermost_context(span)?;
        let exported = self.slot_is_exported(name);
        let descriptor = &mut self.scope_descriptors[usize::from(ctx.descriptor)];
        let slot = u16::try_from(descriptor.slots.len())
            .ok()
            .filter(|slot| *slot <= ContextCoord::MAX_SLOT)
            .ok_or_else(|| CompileError::Unsupported {
                node: format!(
                    "scope holds more than {} context slots",
                    u32::from(ContextCoord::MAX_SLOT) + 1
                ),
                span,
            })?;
        descriptor.slots.push(SlotDescriptor {
            name: name.to_string(),
            kind,
            exported,
        });
        Ok((ctx.reg, slot))
    }

    /// Whether a slot declared now in the innermost scope is a locally
    /// exported module binding.
    fn slot_is_exported(&self, name: &str) -> bool {
        let Some(scope) = self.scopes.last() else {
            return false;
        };
        if scope.kind != ScopeKind::Module {
            return false;
        }
        self.module_state.as_ref().is_some_and(|state| {
            state.exported_names.contains(name) || state.reexport_local_targets.contains_key(name)
        })
    }

    fn ensure_binding_name_available(
        &self,
        name: &str,
        span: (u32, u32),
    ) -> Result<(), CompileError> {
        if self
            .scopes
            .last()
            .expect("binding declaration called outside any scope")
            .bindings
            .contains_key(name)
        {
            return Err(CompileError::Unsupported {
                node: format!("redeclaration of `{name}` in same scope"),
                span,
            });
        }
        Ok(())
    }

    fn insert_binding_info(&mut self, name: &str, info: BindingInfo) {
        let scope = self
            .scopes
            .last_mut()
            .expect("binding declaration called outside any scope");
        scope.bindings.insert(name.to_string(), info);
    }

    /// Declare `name` in the innermost scope: a context slot when the
    /// capture pre-pass (or mapped arguments) requires one, else a fresh
    /// register.
    pub(crate) fn declare_binding(
        &mut self,
        name: &str,
        kind: SlotKind,
        span: (u32, u32),
    ) -> Result<BindingStorage, CompileError> {
        self.declare_binding_with_capture(name, kind, span, true)
    }

    /// [`Self::declare_binding`] where `allow_capture = false` keeps the
    /// binding in a register regardless of the capture set.
    pub(crate) fn declare_binding_with_capture(
        &mut self,
        name: &str,
        kind: SlotKind,
        span: (u32, u32),
        allow_capture: bool,
    ) -> Result<BindingStorage, CompileError> {
        self.ensure_binding_name_available(name, span)?;
        let storage = if allow_capture && self.name_needs_slot(name) {
            let (ctx, slot) = self.allocate_slot(name, kind, span)?;
            BindingStorage::Slot { ctx, slot }
        } else {
            let reg = self.alloc_scratch();
            BindingStorage::Register { reg }
        };
        self.insert_binding_info(name, BindingInfo::new(storage, kind));
        Ok(storage)
    }

    /// Declare `name` in a context slot of the innermost scope whatever
    /// the capture set says. Used for compiler-internal bindings that
    /// nested functions reach by name (class homes, private names, the
    /// module environment) and for bindings a direct eval may see.
    pub(crate) fn declare_forced_slot(
        &mut self,
        name: &str,
        kind: SlotKind,
        span: (u32, u32),
    ) -> Result<BindingStorage, CompileError> {
        self.ensure_binding_name_available(name, span)?;
        let (ctx, slot) = self.allocate_slot(name, kind, span)?;
        let storage = BindingStorage::Slot { ctx, slot };
        self.insert_binding_info(name, BindingInfo::new(storage, kind));
        Ok(storage)
    }

    /// Declare one simple formal parameter over its incoming ABI register,
    /// or in a slot when captured / mapped.
    pub(crate) fn declare_parameter_binding(
        &mut self,
        name: &str,
        argument_register: u16,
        kind: SlotKind,
        span: (u32, u32),
    ) -> Result<BindingStorage, CompileError> {
        debug_assert!(
            argument_register < self.scratch,
            "parameter register must be inside the reserved argument window"
        );
        self.ensure_binding_name_available(name, span)?;
        let storage = if self.name_needs_slot(name) {
            let (ctx, slot) = self.allocate_slot(name, kind, span)?;
            BindingStorage::Slot { ctx, slot }
        } else {
            BindingStorage::Register {
                reg: argument_register,
            }
        };
        self.insert_binding_info(name, BindingInfo::new(storage, kind));
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

    /// As [`Self::lookup_binding`], also reporting the scope index the
    /// name resolved in.
    pub(crate) fn lookup_binding_with_scope(&self, name: &str) -> Option<(BindingInfo, usize)> {
        for (idx, scope) in self.scopes.iter().enumerate().rev() {
            if let Some(info) = scope.bindings.get(name) {
                return Some((*info, idx));
            }
        }
        None
    }

    /// Look up `name` only in the *innermost* scope.
    pub(crate) fn lookup_in_current_scope(&self, name: &str) -> Option<BindingInfo> {
        self.scopes
            .last()
            .and_then(|scope| scope.bindings.get(name).copied())
    }

    /// Flag `name`'s innermost binding as a §10.2.11 named function
    /// expression self-name (immutable; sloppy writes silently drop,
    /// strict writes throw TypeError).
    pub(crate) fn mark_fn_self_name(&mut self, name: &str) {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(info) = scope.bindings.get_mut(name) {
                info.fn_self_name = true;
                return;
            }
        }
    }

    /// Flip a binding's `initialized` flag once its initializer's store
    /// was emitted. The flag is a straight-line fact: it never flips
    /// back and never merges branch states.
    pub(crate) fn mark_initialized(&mut self, name: &str) {
        for scope in self.scopes.iter_mut().rev() {
            if let Some(info) = scope.bindings.get_mut(name) {
                info.initialized = true;
                return;
            }
        }
    }

    /// Read an own binding whose initialization dominates this point: a
    /// register copy or an unchecked slot read.
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
            BindingStorage::Slot { ctx, slot } => self.emit_ctx(
                Op::LoadContextSlot,
                vec![
                    Operand::Register(dst),
                    Operand::Register(0),
                    Operand::Imm32(own_coord(slot)),
                ],
                1,
                ctx,
                span,
            ),
        }
    }

    /// Read an own binding with the TDZ check a slot can carry.
    pub(crate) fn emit_load_storage_checked(
        &mut self,
        dst: u16,
        storage: BindingStorage,
        span: (u32, u32),
    ) {
        match storage {
            BindingStorage::Register { .. } => self.emit_load_storage(dst, storage, span),
            BindingStorage::Slot { ctx, slot } => self.emit_ctx(
                Op::LoadContextSlotChecked,
                vec![
                    Operand::Register(dst),
                    Operand::Register(0),
                    Operand::Imm32(own_coord(slot)),
                ],
                1,
                ctx,
                span,
            ),
        }
    }

    /// Binding initialization: write `src` into the binding, replacing any
    /// TDZ hole.
    pub(crate) fn emit_store_storage(
        &mut self,
        src: u16,
        storage: BindingStorage,
        span: (u32, u32),
    ) {
        match storage {
            BindingStorage::Register { reg } => {
                if src != reg {
                    self.emit(
                        Op::StoreLocal,
                        [Operand::Register(src), Operand::Imm32(reg as i32)],
                        span,
                    );
                }
            }
            BindingStorage::Slot { ctx, slot } => self.emit_ctx(
                Op::StoreContextSlot,
                vec![
                    Operand::Register(src),
                    Operand::Register(0),
                    Operand::Imm32(own_coord(slot)),
                ],
                1,
                ctx,
                span,
            ),
        }
    }

    /// Assignment to an own binding whose initialization does not dominate
    /// this point: a slot store checks the TDZ hole.
    pub(crate) fn emit_store_storage_checked(
        &mut self,
        src: u16,
        storage: BindingStorage,
        span: (u32, u32),
    ) {
        match storage {
            BindingStorage::Register { .. } => self.emit_store_storage(src, storage, span),
            BindingStorage::Slot { ctx, slot } => self.emit_ctx(
                Op::StoreContextSlotChecked,
                vec![
                    Operand::Register(src),
                    Operand::Register(0),
                    Operand::Imm32(own_coord(slot)),
                ],
                1,
                ctx,
                span,
            ),
        }
    }

    /// Whether an own-binding access in scope `scope_index` must carry the
    /// TDZ check even though the binding is statically initialized: a
    /// switch CaseBlock's clauses are entered by jumps that bypass earlier
    /// clauses' declarations.
    pub(crate) fn own_access_needs_check(&self, info: &BindingInfo, scope_index: usize) -> bool {
        info.kind.initial_hole()
            && self
                .scopes
                .get(scope_index)
                .is_some_and(|scope| scope.kind == ScopeKind::Switch)
    }
}

/// Packed coordinate of an own-scope slot (depth 0).
pub(crate) fn own_coord(slot: u16) -> i32 {
    ContextCoord { depth: 0, slot }.to_imm32()
}
