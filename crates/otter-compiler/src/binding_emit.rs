//! Bytecode emission for statically resolved name references.
//!
//! Given a [`Resolved`] binding or a global [`NameRef`], these helpers emit
//! the matching read, assignment, `typeof`, `delete`, or §13.15.2
//! resolve-then-store sequence: register moves for own registers, context
//! slot operations at a static coordinate for slots, and `Lookup*`
//! operations when a sloppy direct eval's extension may intercept the name.
//!
//! # Contents
//! - name load / assignment / `delete` emission and `ResolveLookupRef` /
//!   `StoreRef` pairs.
//! - [`VarTarget`] stores for eval and Annex B variable-scope writes.
//! - `this` reads and `super()` binds through a `DerivedThis` slot.
//! - module export mirroring decided from the resolved module binding.
//!
//! # Invariants
//! - A statically uninitialized own binding reads or writes as `TdzError`;
//!   an outer slot whose kind can hold the hole uses the checked form.
//! - Immutable bindings reject assignment after the right-hand side ran,
//!   with a TDZ check first when the slot can still hold the hole.
//! - Only a store to a locally exported module-scope binding mirrors onto
//!   the module environment.
//!
//! # See also
//! - `compiler` for resolution and the static chain walk.

use crate::compiler::{Access, MODULE_ENV_BINDING, NameRef, Resolved, ScopeLocation, VarTarget};
use crate::scope::CtxReg;
use crate::*;
use otter_bytecode::{
    BindingStoreFallback, ContextCoord, LookupGlobalMode, LookupRefTarget, ScopeKind, SlotKind,
    StoreRefMode,
};

impl Compiler {
    // ------------------------------------------------------------------
    // Emission of resolved references
    // ------------------------------------------------------------------

    fn coord(depth: u16, slot: u16) -> i32 {
        ContextCoord { depth, slot }.to_imm32()
    }

    /// Emit a read of `name` into `dst`. `typeof_mode` makes an
    /// unresolvable name read `undefined` instead of throwing.
    pub(crate) fn emit_name_load(
        &mut self,
        dst: u16,
        name: &str,
        reference: &NameRef,
        typeof_mode: bool,
        span: (u32, u32),
    ) {
        match reference {
            NameRef::Binding(resolved) => self.emit_binding_load(dst, name, resolved, span),
            NameRef::Global(lookup) => {
                let name_idx = self.intern_string_constant(name);
                match lookup {
                    Some(site) => {
                        let op = if typeof_mode {
                            Op::TypeofLookupGlobal
                        } else {
                            Op::LoadLookupGlobal
                        };
                        self.emit_ctx(
                            op,
                            vec![
                                Operand::Register(dst),
                                Operand::Register(0),
                                Operand::ConstIndex(name_idx),
                                Operand::Imm32(i32::from(site.depth)),
                            ],
                            1,
                            site.base,
                            span,
                        );
                    }
                    None => {
                        let op = if typeof_mode {
                            Op::LoadGlobalOrUndefined
                        } else {
                            Op::LoadGlobalOrThrow
                        };
                        self.emit(
                            op,
                            [Operand::Register(dst), Operand::ConstIndex(name_idx)],
                            span,
                        );
                    }
                }
            }
        }
    }

    /// Read a resolved binding into `dst`.
    pub(crate) fn emit_binding_load(
        &mut self,
        dst: u16,
        name: &str,
        resolved: &Resolved,
        span: (u32, u32),
    ) {
        let info = resolved.info;
        if let (Access::Own, false) = (resolved.access, info.initialized) {
            // Statically before the binding's initialization.
            self.emit(
                Op::TdzError,
                [Operand::Imm32(info.storage.diagnostic_index())],
                span,
            );
            return;
        }
        if let (Some(site), Some(slot)) = (resolved.lookup, resolved.slot()) {
            let name_idx = self.intern_string_constant(name);
            self.emit_ctx(
                Op::LoadLookupSlot,
                vec![
                    Operand::Register(dst),
                    Operand::Register(0),
                    Operand::ConstIndex(name_idx),
                    Operand::Imm32(Self::coord(site.depth, slot)),
                ],
                1,
                site.base,
                span,
            );
            return;
        }
        match resolved.access {
            Access::Own => {
                let scope_index = match resolved.location {
                    ScopeLocation::Frame { scope, .. } => scope,
                    ScopeLocation::Chain { .. } => 0,
                };
                if self.own_access_needs_check(&info, scope_index) {
                    self.emit_load_storage_checked(dst, info.storage, span);
                } else {
                    self.emit_load_storage(dst, info.storage, span);
                }
            }
            Access::Outer { depth, slot } => {
                let op = if info.kind.initial_hole() {
                    Op::LoadContextSlotChecked
                } else {
                    Op::LoadContextSlot
                };
                self.emit_ctx(
                    op,
                    vec![
                        Operand::Register(dst),
                        Operand::Register(0),
                        Operand::Imm32(Self::coord(depth, slot)),
                    ],
                    1,
                    CtxReg::Closure,
                    span,
                );
            }
        }
    }

    /// The frame register of a mutable own binding that an update can rewrite
    /// in place: no eval extension can intercept the name, its initialization
    /// dominates this point, and a store into it needs no check.
    pub(crate) fn plain_register_binding(&self, reference: &NameRef) -> Option<u16> {
        let NameRef::Binding(resolved) = reference else {
            return None;
        };
        if resolved.lookup.is_some()
            || resolved.access != Access::Own
            || !resolved.info.initialized
            || self.store_fallback(&resolved.info) != BindingStoreFallback::Mutable
        {
            return None;
        }
        match resolved.info.storage {
            crate::scope::BindingStorage::Register { reg } => Some(reg),
            crate::scope::BindingStorage::Slot { .. } => None,
        }
    }

    /// Store fallback of a resolved binding under the current strictness.
    fn store_fallback(&self, info: &BindingInfo) -> BindingStoreFallback {
        if info.fn_self_name {
            if self.is_strict {
                BindingStoreFallback::ImmutableThrow
            } else {
                BindingStoreFallback::ImmutableIgnore
            }
        } else if info.is_const || matches!(info.kind, SlotKind::Const | SlotKind::Class) {
            BindingStoreFallback::ImmutableThrow
        } else {
            BindingStoreFallback::Mutable
        }
    }

    /// PutValue of `value` into `name` (§9.1.1.1.5 SetMutableBinding /
    /// §9.1.1.4.5), resolving the reference at the store.
    pub(crate) fn emit_name_assign(
        &mut self,
        value: u16,
        name: &str,
        reference: &NameRef,
        span: (u32, u32),
    ) {
        match reference {
            NameRef::Binding(resolved) => self.emit_binding_assign(value, name, resolved, span),
            NameRef::Global(lookup) => {
                let name_idx = self.intern_string_constant(name);
                match lookup {
                    Some(site) => {
                        let mode = LookupGlobalMode {
                            depth: site.depth,
                            strict: self.is_strict,
                        }
                        .to_imm32();
                        self.emit_ctx(
                            Op::StoreLookupGlobal,
                            vec![
                                Operand::Register(value),
                                Operand::Register(0),
                                Operand::ConstIndex(name_idx),
                                Operand::Imm32(mode),
                            ],
                            1,
                            site.base,
                            span,
                        );
                    }
                    None => {
                        let strict = i32::from(self.is_strict);
                        self.emit(
                            Op::StoreGlobalBinding,
                            [
                                Operand::Register(value),
                                Operand::ConstIndex(name_idx),
                                Operand::Imm32(strict),
                            ],
                            span,
                        );
                    }
                }
            }
        }
    }

    /// Assignment to a resolved binding.
    pub(crate) fn emit_binding_assign(
        &mut self,
        value: u16,
        name: &str,
        resolved: &Resolved,
        span: (u32, u32),
    ) {
        let info = resolved.info;
        let fallback = self.store_fallback(&info);
        if let (Some(site), Some(slot)) = (resolved.lookup, resolved.slot()) {
            let name_idx = self.intern_string_constant(name);
            self.emit_ctx(
                Op::StoreLookupSlot,
                vec![
                    Operand::Register(value),
                    Operand::Register(0),
                    Operand::ConstIndex(name_idx),
                    Operand::Imm32(Self::coord(site.depth, slot)),
                    Operand::Imm32(fallback.to_imm32()),
                ],
                1,
                site.base,
                span,
            );
            return;
        }
        // §6.2.5.5 — a binding still in its TDZ raises ReferenceError
        // before any immutability TypeError.
        if resolved.access == Access::Own && !info.initialized {
            self.emit(
                Op::TdzError,
                [Operand::Imm32(info.storage.diagnostic_index())],
                span,
            );
            return;
        }
        match fallback {
            BindingStoreFallback::ImmutableThrow => {
                if let Access::Outer { depth, slot } = resolved.access
                    && info.kind.initial_hole()
                {
                    let probe = self.alloc_scratch();
                    self.emit_ctx(
                        Op::LoadContextSlotChecked,
                        vec![
                            Operand::Register(probe),
                            Operand::Register(0),
                            Operand::Imm32(Self::coord(depth, slot)),
                        ],
                        1,
                        CtxReg::Closure,
                        span,
                    );
                }
                crate::assignment::emit_assignment_type_error(
                    self,
                    &format!("Assignment to constant variable '{name}'."),
                    span,
                );
                return;
            }
            BindingStoreFallback::ImmutableIgnore => return,
            BindingStoreFallback::Mutable => {}
        }
        match resolved.access {
            Access::Own => {
                let scope_index = match resolved.location {
                    ScopeLocation::Frame { scope, .. } => scope,
                    ScopeLocation::Chain { .. } => 0,
                };
                if self.own_access_needs_check(&info, scope_index) {
                    self.emit_store_storage_checked(value, info.storage, span);
                } else {
                    self.emit_store_storage(value, info.storage, span);
                }
            }
            Access::Outer { depth, slot } => {
                let op = if info.kind.initial_hole() {
                    Op::StoreContextSlotChecked
                } else {
                    Op::StoreContextSlot
                };
                self.emit_ctx(
                    op,
                    vec![
                        Operand::Register(value),
                        Operand::Register(0),
                        Operand::Imm32(Self::coord(depth, slot)),
                    ],
                    1,
                    CtxReg::Closure,
                    span,
                );
            }
        }
    }

    /// §13.15.2 — resolve an assignment target's base BEFORE its
    /// right-hand side runs, when an eval extension may intercept it.
    /// Returns the engine-internal reference register for
    /// [`Self::emit_store_ref`].
    pub(crate) fn emit_resolve_ref(
        &mut self,
        name: &str,
        reference: &NameRef,
        span: (u32, u32),
    ) -> Option<u16> {
        let site = reference.lookup()?;
        let target = match reference {
            NameRef::Binding(resolved) => LookupRefTarget::Slot(ContextCoord {
                depth: site.depth,
                slot: resolved.slot()?,
            }),
            NameRef::Global(_) => LookupRefTarget::Global { depth: site.depth },
        };
        let name_idx = self.intern_string_constant(name);
        let reg = self.alloc_scratch();
        self.emit_ctx(
            Op::ResolveLookupRef,
            vec![
                Operand::Register(reg),
                Operand::Register(0),
                Operand::ConstIndex(name_idx),
                Operand::Imm32(target.to_imm32()),
            ],
            1,
            site.base,
            span,
        );
        Some(reg)
    }

    /// PutValue through a reference resolved by [`Self::emit_resolve_ref`].
    pub(crate) fn emit_store_ref(
        &mut self,
        value: u16,
        ref_reg: u16,
        name: &str,
        reference: &NameRef,
        span: (u32, u32),
    ) {
        let (slot, fallback) = match reference {
            NameRef::Binding(resolved) => (resolved.slot(), self.store_fallback(&resolved.info)),
            NameRef::Global(_) => (None, BindingStoreFallback::Mutable),
        };
        let mode = StoreRefMode {
            slot,
            fallback,
            strict: self.is_strict,
        }
        .to_imm32();
        let name_idx = self.intern_string_constant(name);
        self.emit(
            Op::StoreRef,
            [
                Operand::Register(value),
                Operand::Register(ref_reg),
                Operand::ConstIndex(name_idx),
                Operand::Imm32(mode),
            ],
            span,
        );
    }

    /// Store into a variable-scope target (function hoisting and Annex B
    /// sync inside a sloppy eval; own-frame var bindings elsewhere).
    pub(crate) fn emit_var_target_store(
        &mut self,
        value: u16,
        name: &str,
        target: VarTarget,
        span: (u32, u32),
    ) {
        match target {
            VarTarget::Own(storage) => self.emit_store_storage(value, storage, span),
            VarTarget::Outer { depth, slot } => self.emit_ctx(
                Op::StoreContextSlot,
                vec![
                    Operand::Register(value),
                    Operand::Register(0),
                    Operand::Imm32(Self::coord(depth, slot)),
                ],
                1,
                CtxReg::Closure,
                span,
            ),
            VarTarget::Extension { depth } => {
                let name_idx = self.intern_string_constant(name);
                self.emit_ctx(
                    Op::StoreVarScope,
                    vec![
                        Operand::Register(value),
                        Operand::Register(0),
                        Operand::ConstIndex(name_idx),
                        Operand::Imm32(i32::from(depth)),
                    ],
                    1,
                    CtxReg::Closure,
                    span,
                );
            }
        }
    }

    /// `delete name` (§13.5.1.2 / §9.1.1.1.7): declarative bindings are
    /// not deletable; eval-created ones are.
    pub(crate) fn emit_name_delete(
        &mut self,
        dst: u16,
        name: &str,
        reference: &NameRef,
        span: (u32, u32),
    ) {
        let name_idx = self.intern_string_constant(name);
        match reference {
            NameRef::Binding(resolved) => match resolved.lookup {
                Some(site) => self.emit_ctx(
                    Op::DeleteLookupSlot,
                    vec![
                        Operand::Register(dst),
                        Operand::Register(0),
                        Operand::ConstIndex(name_idx),
                        Operand::Imm32(i32::from(site.depth)),
                    ],
                    1,
                    site.base,
                    span,
                ),
                None => self.emit(Op::LoadFalse, [Operand::Register(dst)], span),
            },
            NameRef::Global(Some(site)) => self.emit_ctx(
                Op::DeleteLookupGlobal,
                vec![
                    Operand::Register(dst),
                    Operand::Register(0),
                    Operand::ConstIndex(name_idx),
                    Operand::Imm32(i32::from(site.depth)),
                ],
                1,
                site.base,
                span,
            ),
            NameRef::Global(None) => {
                if self.script_global_lexicals.contains(name) {
                    self.emit(Op::LoadFalse, [Operand::Register(dst)], span);
                } else {
                    // §9.1.1.4.7 — delete the global object property.
                    let global_reg = self.alloc_scratch();
                    self.emit(Op::LoadGlobalThis, [Operand::Register(global_reg)], span);
                    self.emit(
                        Op::DeleteProperty,
                        vec![
                            Operand::Register(dst),
                            Operand::Register(global_reg),
                            Operand::ConstIndex(name_idx),
                        ],
                        span,
                    );
                }
            }
        }
    }

    /// Load a compiler-internal binding (`%home`, `#name`, …) resolved by
    /// name, or `None` when no enclosing scope declares it.
    pub(crate) fn load_internal(&mut self, name: &str, span: (u32, u32)) -> Option<u16> {
        let resolved = self.resolve_name(name)?;
        let dst = self.alloc_scratch();
        self.emit_binding_load(dst, name, &resolved, span);
        Some(dst)
    }

    /// Load `name` as declared in the scope at `location`.
    pub(crate) fn load_at(
        &mut self,
        location: ScopeLocation,
        name: &str,
        span: (u32, u32),
    ) -> Option<u16> {
        let resolved = self.resolve_at(location, name)?;
        let dst = self.alloc_scratch();
        self.emit_binding_load(dst, name, &resolved, span);
        Some(dst)
    }

    // ------------------------------------------------------------------
    // `this` and `super()`
    // ------------------------------------------------------------------

    /// The binding of a derived constructor's `this` the current code
    /// observes, when it lives in a `DerivedThis` slot: arrows (and a
    /// `super()`-capable direct eval) are transparent for `this`.
    fn derived_this_binding(&self) -> Option<Resolved> {
        for frame_index in (0..self.stack.len()).rev() {
            let frame = &self.stack[frame_index];
            if frame.is_arrow {
                continue;
            }
            if frame_index == 0 && frame.eval_this_from_chain {
                let chain = self.eval_chain.as_ref()?;
                let hop = chain.scopes.iter().position(|scope| {
                    scope
                        .descriptor
                        .slots
                        .iter()
                        .any(|slot| slot.kind == SlotKind::DerivedThis)
                })?;
                return self.resolve_at(ScopeLocation::Chain { hop }, "this");
            }
            let derived = frame.derived_this?;
            let scope = frame.scopes.iter().position(|scope| {
                scope.bindings.get("this").is_some_and(|info| {
                    info.storage
                        == BindingStorage::Slot {
                            ctx: CtxReg::Reg(derived.ctx),
                            slot: derived.slot,
                        }
                })
            })?;
            return self.resolve_at(
                ScopeLocation::Frame {
                    frame: frame_index,
                    scope,
                },
                "this",
            );
        }
        None
    }

    /// Read the current `this` binding into `dst`.
    pub(crate) fn emit_load_this(&mut self, dst: u16, span: (u32, u32)) {
        match self.derived_this_binding() {
            Some(resolved) => match resolved.access {
                Access::Own => self.emit_load_storage_checked(dst, resolved.info.storage, span),
                Access::Outer { depth, slot } => self.emit_ctx(
                    Op::LoadContextSlotChecked,
                    vec![
                        Operand::Register(dst),
                        Operand::Register(0),
                        Operand::Imm32(Self::coord(depth, slot)),
                    ],
                    1,
                    CtxReg::Closure,
                    span,
                ),
            },
            None => self.emit(Op::LoadThis, [Operand::Register(dst)], span),
        }
    }

    /// BindThisValue (§9.1.1.3.1) after a `super(...)` construct.
    pub(crate) fn emit_bind_this(&mut self, value: u16, span: (u32, u32)) {
        match self.derived_this_binding() {
            Some(resolved) => match resolved.access {
                Access::Own => {
                    let BindingStorage::Slot { ctx, slot } = resolved.info.storage else {
                        unreachable!("DerivedThis is a context slot");
                    };
                    self.emit_ctx(
                        Op::BindThisContextSlot,
                        vec![
                            Operand::Register(value),
                            Operand::Register(0),
                            Operand::Imm32(Self::coord(0, slot)),
                        ],
                        1,
                        ctx,
                        span,
                    );
                    // Keep the frame's own `this` in step for operations that
                    // read the receiver implicitly (`super.x`).
                    self.emit(Op::BindThisValue, [Operand::Register(value)], span);
                }
                Access::Outer { depth, slot } => self.emit_ctx(
                    Op::BindThisContextSlot,
                    vec![
                        Operand::Register(value),
                        Operand::Register(0),
                        Operand::Imm32(Self::coord(depth, slot)),
                    ],
                    1,
                    CtxReg::Closure,
                    span,
                ),
            },
            None => self.emit(Op::BindThisValue, [Operand::Register(value)], span),
        }
    }

    // ------------------------------------------------------------------
    // Module export mirroring
    // ------------------------------------------------------------------

    /// Mirror a store to `name` onto the module environment when `name`
    /// resolves to a locally exported module-scope binding — decided from
    /// the resolved binding, so a shadowing local never mirrors.
    pub(crate) fn emit_module_export_mirror(
        &mut self,
        name: &str,
        value_reg: u16,
        span: (u32, u32),
    ) {
        let Some(resolved) = self.resolve_name(name) else {
            return;
        };
        let (exported, aliases, env_location) = match resolved.location {
            ScopeLocation::Frame { frame, scope: 0 } => {
                let Some(state) = self.stack[frame].module_state.as_ref() else {
                    return;
                };
                (
                    state.exported_names.contains(name),
                    state
                        .reexport_local_targets
                        .get(name)
                        .cloned()
                        .unwrap_or_default(),
                    resolved.location,
                )
            }
            ScopeLocation::Chain { hop } => {
                let Some(scope) = self.eval_chain.as_ref().and_then(|c| c.scopes.get(hop)) else {
                    return;
                };
                if scope.descriptor.kind != ScopeKind::Module {
                    return;
                }
                let exported = scope
                    .descriptor
                    .slots
                    .iter()
                    .any(|slot| slot.name == name && slot.exported);
                (exported, Vec::new(), resolved.location)
            }
            ScopeLocation::Frame { .. } => return,
        };
        if !exported && aliases.is_empty() {
            return;
        }
        let Some(env_reg) = self.load_at(env_location, MODULE_ENV_BINDING, span) else {
            return;
        };
        if exported {
            self.emit_store_property(env_reg, name, value_reg, span);
        }
        for alias in &aliases {
            self.emit_store_property(env_reg, alias, value_reg, span);
        }
    }

    /// Mirror `value_reg` onto `module_env.default`.
    pub(crate) fn emit_module_export_default_mirror(&mut self, value_reg: u16, span: (u32, u32)) {
        if self
            .stack
            .first()
            .and_then(|f| f.module_state.as_ref())
            .is_none()
        {
            return;
        }
        let Some(env_reg) = self.load_internal(MODULE_ENV_BINDING, span) else {
            return;
        };
        self.emit_store_property(env_reg, "default", value_reg, span);
    }

    /// Load the module environment object.
    pub(crate) fn load_module_env(&mut self, span: (u32, u32)) -> Option<u16> {
        self.load_internal(MODULE_ENV_BINDING, span)
    }
}
