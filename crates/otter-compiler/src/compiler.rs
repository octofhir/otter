//! Compiler stack wrapper for nested function lowering.
//!
//! # Contents
//! - context stack management
//! - private-name namespace stack
//! - `Deref` access to the current function context
//!
//! # Invariants
//! - The stack is never empty while lowering.
//!
//! # See also
//! - `function_context` for per-function state

use crate::*;

/// Compile-time stack of function contexts. The innermost context
/// is at the top; capture resolution walks this stack downward to
/// find a binding declared by an ancestor.
///
/// The compiler exposes the inner-most [`FunctionContext`] through
/// `Deref` / `DerefMut` so existing code continues to use `cx.emit`,
/// `cx.scratch`, etc. without referencing the stack explicitly.
#[derive(Debug)]
pub(crate) struct Compiler {
    pub(crate) stack: Vec<FunctionContext>,
    /// One-shot hint set by the NAMED FunctionExpression lowering:
    /// the next `compile_function_full` marks its self-name binding
    /// as the §10.2.11 immutable function-expression name (strict
    /// assignment throws TypeError; sloppy writes are dropped).
    /// Function declarations leave this false — their name resolves
    /// to the outer mutable var binding.
    pub(crate) fn_self_immutable_hint: bool,
    /// One-shot hint set by MethodDefinition lowering (class and
    /// object-literal methods / accessors): the next
    /// `compile_function_full` marks its record `is_method`, so the
    /// runtime never gives it an implicit `prototype` property
    /// (§10.2.5 MakeConstructor skips methods).
    pub(crate) next_fn_is_method: bool,
    /// One-shot hint paired with [`Self::next_fn_is_method`]: the
    /// next `compile_function_full` resolves `super.x` through the
    /// statics-side home object (`static` MethodDefinition bodies).
    pub(crate) next_fn_static_home: bool,
    /// One-shot hint set by class-constructor lowering: the next
    /// `compile_function_full` skips the function-expression-style
    /// self-name binding (the class name resolves through the class
    /// scope) without marking the record as a MethodDefinition (a
    /// class constructor IS a constructor).
    pub(crate) next_fn_no_self_name: bool,
    /// One-shot byte range for the next compiled function's §20.2.3.5
    /// [[SourceText]] when it differs from the function-body span — a
    /// concise method / accessor reports its enclosing
    /// `MethodDefinition` (key and prefixes included). Consumed by
    /// `compile_function_full`.
    pub(crate) next_fn_source_text_span: Option<(u32, u32)>,
    /// Stack of private-field namespace ids — one per enclosing
    /// class declaration. The top entry is the namespace used to
    /// mangle every `#name` reference inside the current class
    /// body. Empty when no class encloses the current expression
    /// (in which case `#name` references are a syntax error).
    /// Each entry is the integer suffix of `__priv_<n>_<name>`
    /// so peers across classes never collide.
    /// <https://tc39.es/ecma262/#sec-private-names>
    pub(crate) private_namespaces: Vec<u32>,
    /// Private names declared by each enclosing class, parallel to
    /// `private_namespaces`. Seeds §8.2.4 AllPrivateNamesValid when a
    /// nested class body is compiled (its `#name` references may
    /// resolve to an outer class).
    pub(crate) class_private_names: Vec<std::collections::HashSet<String>>,
    /// Declaration-ordered private names per enclosing class —
    /// parallel to `private_namespaces`; indexes the per-class
    /// `__privarr_{ns}` symbol array.
    pub(crate) class_private_ordered: Vec<Vec<String>>,
    /// Instance private METHOD / ACCESSOR names per enclosing class
    /// (parallel to `private_namespaces`). Access to these emits a
    /// §7.3.31 brand check — the prototype-side lookup alone must
    /// not satisfy access before `super()` installs the brand.
    pub(crate) class_private_instance_methods: Vec<std::collections::HashSet<String>>,
    /// `true` when compiling any `eval` body — §B.3.3.3 makes the
    /// Annex B global function extension *deletable* for eval code
    /// (CreateGlobalVarBinding(F, true)) where script code creates a
    /// non-configurable binding.
    pub(crate) in_eval: bool,
    /// `true` when compiling a *strict* `eval` body: §19.2.1.1 gives
    /// strict eval its own variable environment, so top-level `var` /
    /// `function` declarations must NOT mirror onto the global object
    /// (ordinary scripts mirror per §16.1.7 regardless of strictness).
    pub(crate) suppress_global_mirror: bool,
    /// §16.1.7 GlobalDeclarationInstantiation — names of script
    /// top-level `var` and function declarations. These live as
    /// global-object properties (the global environment's object
    /// record), not `<main>` locals: every read and write — from the
    /// script body, nested functions, sibling scripts, and eval
    /// chunks — resolves through the global object, so none of them
    /// can observe a stale copy. Empty for modules and eval bodies.
    pub(crate) script_global_vars: std::collections::HashSet<String>,
    /// §9.1.1.4 global declarative record — names of script
    /// top-level `let` / `const` / `class` declarations. These live
    /// in the interpreter's realm-wide lexical map (shared across
    /// sibling scripts, shadowing global object properties), not as
    /// `<main>` locals. Empty for modules and eval bodies (eval
    /// lexicals are private to the eval, §19.2.1.1).
    pub(crate) script_global_lexicals: std::collections::HashSet<String>,
    /// `true` while lowering class instance-field initializers
    /// (which compile into the constructor frame). A direct eval
    /// there may use `new.target` but observes `undefined`
    /// (§15.7.10 — field initializers are their own function-like
    /// code with no [[NewTarget]]). Cleared on entry to any nested
    /// non-arrow function.
    pub(crate) in_field_initializer: bool,
    /// `true` when this eval body's caller permits `new.target` —
    /// inherited so a nested direct eval keeps the signal
    /// (§19.2.1.1 step 5).
    pub(crate) eval_new_target_allowed: bool,
    /// Memoized `expr_number_typed` results, keyed by AST-node address.
    ///
    /// Type hints are read top-down at every binary operator, so a chained
    /// arithmetic expression would re-walk each subtree once per enclosing
    /// operator. Nodes live in the parse arena for the whole compile, so their
    /// addresses are stable keys. Purely a compile-time cache; dropping an
    /// entry would only cost time.
    pub(crate) number_typed_cache: RefCell<HashMap<usize, bool>>,
    /// Interned annotation names referenced by [`TypeHint::Class`].
    pub(crate) class_hint_names: Vec<String>,
    /// Reverse index into [`Self::class_hint_names`].
    pub(crate) class_hint_name_ids: HashMap<String, u32>,
    /// Module-wide `class` declarations: name → constructor function id, or
    /// `None` once a second class of the same name is seen. An ambiguous name
    /// has no single instance shape, so its sites are dropped.
    pub(crate) declared_classes: HashMap<String, Option<u32>>,
    /// Class-annotated property sites awaiting name resolution. Resolution is
    /// deferred to the end of the module because a class may be declared after
    /// the function whose parameters name it.
    pub(crate) pending_class_hint_sites: Vec<PendingClassHintSite>,
}

/// One class-annotated property site, still holding the interned annotation
/// name rather than a resolved class.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PendingClassHintSite {
    /// Function whose code contains the site.
    pub(crate) function_id: u32,
    /// Instruction PC of the property access.
    pub(crate) pc: u32,
    /// Index into [`Compiler::class_hint_names`].
    pub(crate) name: u32,
}

impl Compiler {
    pub(crate) fn new(top: FunctionContext) -> Self {
        Self {
            stack: vec![top],
            fn_self_immutable_hint: false,
            next_fn_is_method: false,
            next_fn_static_home: false,
            next_fn_no_self_name: false,
            next_fn_source_text_span: None,
            private_namespaces: Vec::new(),
            class_private_names: Vec::new(),
            class_private_ordered: Vec::new(),
            class_private_instance_methods: Vec::new(),
            suppress_global_mirror: false,
            in_eval: false,
            script_global_vars: std::collections::HashSet::new(),
            script_global_lexicals: std::collections::HashSet::new(),
            in_field_initializer: false,
            eval_new_target_allowed: false,
            number_typed_cache: RefCell::new(HashMap::new()),
            class_hint_names: Vec::new(),
            class_hint_name_ids: HashMap::new(),
            declared_classes: HashMap::new(),
            pending_class_hint_sites: Vec::new(),
        }
    }

    /// Intern one annotation name, returning its stable index.
    pub(crate) fn intern_class_hint_name(&mut self, name: &str) -> u32 {
        if let Some(&id) = self.class_hint_name_ids.get(name) {
            return id;
        }
        let id = self.class_hint_names.len() as u32;
        self.class_hint_names.push(name.to_string());
        self.class_hint_name_ids.insert(name.to_string(), id);
        id
    }

    /// Record that `name` names the class whose constructor is `function_id`.
    /// A repeated name becomes ambiguous and stops seeding anything.
    pub(crate) fn declare_class(&mut self, name: &str, function_id: u32) {
        match self.declared_classes.get_mut(name) {
            Some(slot) => *slot = None,
            None => {
                self.declared_classes
                    .insert(name.to_string(), Some(function_id));
            }
        }
    }

    /// Move a finished function's class-annotated property sites onto the
    /// module-wide list, where they wait for name resolution.
    pub(crate) fn take_class_hint_sites(&mut self, function_id: u32, sites: Vec<(u32, u32)>) {
        self.pending_class_hint_sites
            .extend(sites.into_iter().map(|(pc, name)| PendingClassHintSite {
                function_id,
                pc,
                name,
            }));
    }

    /// `true` when a sloppy function on the compile stack contains a direct
    /// eval call site. Only such eval code can leave a persistent var binding
    /// in an activation's dynamic environment; strict eval owns its fresh
    /// environment and cannot shadow later references.
    pub(crate) fn any_enclosing_leaking_direct_eval(&self) -> bool {
        self.stack
            .iter()
            .any(|frame| frame.contains_direct_eval && !frame.is_strict)
    }

    pub(crate) fn current_private_namespace(&self) -> Option<u32> {
        self.private_namespaces.last().copied()
    }

    pub(crate) fn private_key_binding_name(&self, name: &str) -> Option<String> {
        self.current_private_namespace()
            .map(|ns| format!("__privsym_{ns}_{name}"))
    }

    /// `(fn_depth, scope_depth)` (both 1-based) of the innermost
    /// live declaration of `name` across the whole function-context
    /// stack, or `None` for unresolved / global names. Used by the
    /// `with` probe to decide which object environments are inner
    /// than the static binding (§9.1.1.2.1).
    pub(crate) fn binding_position(&self, name: &str) -> Option<(usize, usize)> {
        for (fi, frame) in self.stack.iter().enumerate().rev() {
            for (si, scope) in frame.scopes.iter().enumerate().rev() {
                if scope.bindings.contains_key(name) {
                    return Some((fi + 1, si + 1));
                }
            }
        }
        None
    }

    pub(crate) fn top_mut(&mut self) -> &mut FunctionContext {
        self.stack
            .last_mut()
            .expect("compiler context stack is empty")
    }

    pub(crate) fn push(&mut self, ctx: FunctionContext) {
        self.stack.push(ctx);
    }

    pub(crate) fn pop(&mut self) -> FunctionContext {
        self.stack
            .pop()
            .expect("compiler pop on empty context stack")
    }

    /// Walk the ancestor chain (excluding the top frame) and resolve
    /// `name` to an absolute upvalue index in the **top** frame's
    /// `frame.upvalues`. Each intermediate ancestor that didn't yet
    /// capture `name` gets a fresh capture slot pointing at the next
    /// ancestor up.
    pub(crate) fn resolve_capture(&mut self, name: &str) -> Option<u16> {
        if self.stack.len() < 2 {
            return None;
        }
        let top_idx = self.stack.len() - 1;
        // Already captured at top?
        if let Some(&idx) = self.stack[top_idx].captured_uv.get(name) {
            return Some(idx);
        }
        // Find the deepest ancestor that has `name` as an
        // own-upvalue (or already-resolved capture). Search from
        // direct-parent (top_idx - 1) downward.
        let mut found: Option<(usize, u16)> = None;
        for i in (0..top_idx).rev() {
            // Local bindings shadow passthrough captures with the
            // same name. This matters for functions with parameter
            // expressions: a default initializer may capture outer
            // `x`, while body `var x` must be captured by closures
            // created in the body.
            let mut hit: Option<u16> = None;
            for scope in self.stack[i].scopes.iter().rev() {
                if let Some(info) = scope.bindings.get(name) {
                    if let BindingStorage::Upvalue { idx } = info.storage {
                        hit = Some(idx);
                    }
                    break;
                }
            }
            if let Some(idx) = hit {
                found = Some((i, idx));
                break;
            }
            // Already-captured upvalue in this ancestor?
            if let Some(&idx) = self.stack[i].captured_uv.get(name) {
                found = Some((i, idx));
                break;
            }
        }
        let (anchor_idx, mut current) = found?;
        // Cascade the cell from anchor down to the top frame: each
        // intermediate ancestor adds a capture entry pointing at the
        // previous one.
        for j in (anchor_idx + 1)..=top_idx {
            let frame = &mut self.stack[j];
            if let Some(&existing) = frame.captured_uv.get(name) {
                current = existing;
                continue;
            }
            // Issue the capture in the VIRTUAL index space — the
            // frame may declare more own captured cells after this
            // resolution, so the absolute `own_count + position`
            // index is only computed at function finalization.
            let new_idx = crate::function_context::VIRTUAL_CAPTURE_BASE
                .checked_add(frame.parent_captures.len() as u16)
                .expect("captured upvalue index overflow");
            frame.parent_captures.push(current as u32);
            frame.captured_uv.insert(name.to_string(), new_idx);
            current = new_idx;
        }
        Some(current)
    }

    /// Resolve the declaration-owner frame and [`BindingInfo`] for a captured
    /// name. Passthrough `captured_uv` entries are deliberately ignored: an
    /// eval record outside the actual declaration cannot shadow that binding.
    pub(crate) fn captured_binding_owner(&self, name: &str) -> Option<(usize, BindingInfo)> {
        for (frame_index, frame) in self.stack.iter().enumerate().rev() {
            for scope in frame.scopes.iter().rev() {
                if let Some(info) = scope.bindings.get(name) {
                    return Some((frame_index, *info));
                }
            }
        }
        None
    }

    /// Resolve one source-level capture, its declaration metadata, and the
    /// exact physical eval-chain prefix allowed to shadow it.
    ///
    /// The declaration owner is excluded and the current frame is included.
    /// A shadow operation is needed only when that slice contains a sloppy
    /// direct-eval site. Once needed, the encoded depth counts every physical
    /// direct-eval record in the slice, including strict empty records that
    /// occupy a parent-chain node.
    pub(crate) fn resolve_capture_with_info(
        &mut self,
        name: &str,
    ) -> Option<(u16, BindingInfo, u32)> {
        let (owner_index, info) = self.captured_binding_owner(name)?;
        let inner_frames = &self.stack[(owner_index + 1)..];
        let may_shadow = inner_frames
            .iter()
            .any(|frame| frame.contains_direct_eval && !frame.is_strict);
        let eval_depth = if may_shadow {
            u32::try_from(
                inner_frames
                    .iter()
                    .filter(|frame| frame.contains_direct_eval)
                    .count(),
            )
            .expect("eval-environment depth overflow")
        } else {
            0
        };
        let index = self.resolve_capture(name)?;
        Some((index, info, eval_depth))
    }

    /// Emit the complete captured-binding read family.
    ///
    /// A live eval environment can be owned by this frame or inherited by a
    /// descendant closure. In that case the runtime must probe the eval chain
    /// before falling back to the statically captured cell.
    pub(crate) fn emit_captured_binding_load(
        &mut self,
        destination: u16,
        name: &str,
        index: u16,
        eval_depth: u32,
        span: (u32, u32),
    ) {
        self.emit_captured_binding_load_snap(destination, name, index, eval_depth, None, span);
    }

    /// [`Self::emit_captured_binding_load`] reading through a pre-resolved
    /// reference: `snapshot` holds the eval-binding sequence captured by
    /// [`Op::EvalBindingSeq`] before the RHS ran (§13.15.2).
    pub(crate) fn emit_captured_binding_load_snap(
        &mut self,
        destination: u16,
        name: &str,
        index: u16,
        eval_depth: u32,
        snapshot: Option<u16>,
        span: (u32, u32),
    ) {
        if eval_depth != 0 {
            let name = self.intern_string_constant(name);
            let depth_imm =
                Operand::Imm32(i32::try_from(eval_depth).expect("eval-environment depth overflow"));
            if let Some(snapshot) = snapshot {
                self.emit(
                    Op::LoadShadowedUpvalueSnap,
                    vec![
                        Operand::Register(destination),
                        Operand::ConstIndex(name),
                        Operand::Imm32(i32::from(index)),
                        depth_imm,
                        Operand::Register(snapshot),
                    ],
                    span,
                );
                return;
            }
            self.emit(
                Op::LoadShadowedUpvalue,
                [
                    Operand::Register(destination),
                    Operand::ConstIndex(name),
                    Operand::Imm32(i32::from(index)),
                    depth_imm,
                ],
                span,
            );
        } else {
            self.emit(
                Op::LoadUpvalue,
                [
                    Operand::Register(destination),
                    Operand::Imm32(i32::from(index)),
                ],
                span,
            );
        }
    }

    /// Emit the complete captured-binding assignment family.
    ///
    /// Shadow lookup intentionally occurs when the store executes, after RHS
    /// evaluation. This matches the live eval-environment behavior of V8 for
    /// plain, compound, logical, destructuring, and update assignments.
    pub(crate) fn emit_captured_binding_store(
        &mut self,
        value: u16,
        name: &str,
        index: u16,
        info: BindingInfo,
        eval_depth: u32,
        span: (u32, u32),
    ) {
        self.emit_captured_binding_store_snap(value, name, index, info, eval_depth, None, span);
    }

    /// [`Self::emit_captured_binding_store`] writing through a pre-resolved
    /// reference (§13.15.2 PutValue): bindings a direct eval introduced
    /// during the RHS are invisible to the bounded lookup.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn emit_captured_binding_store_snap(
        &mut self,
        value: u16,
        name: &str,
        index: u16,
        info: BindingInfo,
        eval_depth: u32,
        snapshot: Option<u16>,
        span: (u32, u32),
    ) {
        use otter_bytecode::opcode_schema::{ShadowedUpvalueFallback, ShadowedUpvalueStorePolicy};

        let fallback = if info.fn_self_name {
            if self.is_strict {
                ShadowedUpvalueFallback::ImmutableThrow
            } else {
                ShadowedUpvalueFallback::ImmutableIgnore
            }
        } else if info.is_const {
            ShadowedUpvalueFallback::ImmutableThrow
        } else {
            ShadowedUpvalueFallback::Mutable
        };
        if eval_depth != 0 {
            let name = self.intern_string_constant(name);
            let policy = ShadowedUpvalueStorePolicy {
                eval_depth,
                fallback,
            }
            .to_imm32()
            .expect("shadowed-upvalue store policy overflow");
            if let Some(snapshot) = snapshot {
                self.emit(
                    Op::StoreShadowedUpvalueCheckedSnap,
                    vec![
                        Operand::Register(value),
                        Operand::ConstIndex(name),
                        Operand::Imm32(i32::from(index)),
                        Operand::Imm32(policy),
                        Operand::Register(snapshot),
                    ],
                    span,
                );
                return;
            }
            self.emit(
                Op::StoreShadowedUpvalueChecked,
                [
                    Operand::Register(value),
                    Operand::ConstIndex(name),
                    Operand::Imm32(i32::from(index)),
                    Operand::Imm32(policy),
                ],
                span,
            );
            return;
        }
        match fallback {
            ShadowedUpvalueFallback::Mutable => self.emit(
                Op::StoreUpvalueChecked,
                [Operand::Register(value), Operand::Imm32(i32::from(index))],
                span,
            ),
            ShadowedUpvalueFallback::ImmutableThrow => {
                crate::assignment::emit_assignment_type_error(
                    self,
                    &format!("Assignment to constant variable '{name}'."),
                    span,
                );
            }
            ShadowedUpvalueFallback::ImmutableIgnore => {}
        }
    }

    /// Mirror an assignment to an exported module binding through to
    /// `module_env`, including assignments emitted inside nested
    /// functions. Nested functions capture the synthetic module-env
    /// binding through the normal upvalue cascade.
    pub(crate) fn emit_module_export_mirror(
        &mut self,
        name: &str,
        value_reg: u16,
        span: (u32, u32),
    ) {
        let exported = self.stack.iter().any(|ctx| {
            ctx.module_state
                .as_ref()
                .is_some_and(|state| state.exported_names.contains(name))
        });
        // Renamed local re-export targets (`export { name as alias }`)
        // are mirrored under their *alias* on every write to `name`, so
        // the aliased export tracks later assignments (live binding).
        let aliases: Vec<String> = self
            .stack
            .iter()
            .filter_map(|ctx| ctx.module_state.as_ref())
            .filter_map(|state| state.reexport_local_targets.get(name))
            .flatten()
            .cloned()
            .collect();
        if !exported && aliases.is_empty() {
            return;
        }
        let env_uv = match self.stack.last().and_then(|ctx| ctx.module_state.as_ref()) {
            Some(state) => state.module_env_uv,
            None => match self.resolve_capture(&module_env_synthetic_name()) {
                Some(idx) => idx,
                None => return,
            },
        };
        let env_reg = self.alloc_scratch();
        self.emit(
            Op::LoadUpvalue,
            [Operand::Register(env_reg), Operand::Imm32(env_uv as i32)],
            span,
        );
        if exported {
            self.emit_store_property(env_reg, name, value_reg, span);
        }
        for alias in &aliases {
            self.emit_store_property(env_reg, alias, value_reg, span);
        }
    }
}

impl std::ops::Deref for Compiler {
    type Target = FunctionContext;
    fn deref(&self) -> &Self::Target {
        self.stack.last().expect("compiler context stack is empty")
    }
}

impl std::ops::DerefMut for Compiler {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.stack
            .last_mut()
            .expect("compiler context stack is empty")
    }
}
