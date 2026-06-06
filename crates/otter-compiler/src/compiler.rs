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
}

impl Compiler {
    pub(crate) fn new(top: FunctionContext) -> Self {
        Self {
            stack: vec![top],
            private_namespaces: Vec::new(),
            class_private_names: Vec::new(),
            suppress_global_mirror: false,
            in_eval: false,
            script_global_vars: std::collections::HashSet::new(),
            script_global_lexicals: std::collections::HashSet::new(),
            in_field_initializer: false,
            eval_new_target_allowed: false,
        }
    }

    pub(crate) fn current_private_namespace(&self) -> Option<u32> {
        self.private_namespaces.last().copied()
    }

    pub(crate) fn private_key_binding_name(&self, name: &str) -> Option<String> {
        self.current_private_namespace()
            .map(|ns| format!("__privsym_{ns}_{name}"))
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
            let new_idx = frame
                .own_upvalue_count
                .checked_add(frame.parent_captures.len() as u16)
                .expect("captured upvalue index overflow");
            frame.parent_captures.push(current as u32);
            frame.captured_uv.insert(name.to_string(), new_idx);
            current = new_idx;
        }
        Some(current)
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
