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
}

impl Compiler {
    pub(crate) fn new(top: FunctionContext) -> Self {
        Self {
            stack: vec![top],
            private_namespaces: Vec::new(),
        }
    }

    pub(crate) fn current_private_namespace(&self) -> Option<u32> {
        self.private_namespaces.last().copied()
    }

    pub(crate) fn mangle_private(&self, name: &str) -> Option<String> {
        self.current_private_namespace()
            .map(|ns| format!("__priv_{ns}_{name}"))
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
