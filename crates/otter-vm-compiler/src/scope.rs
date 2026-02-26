//! Scope management for variable resolution

use std::collections::{HashMap, HashSet};

/// Variable declaration kind
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VariableKind {
    /// var declaration (hoisted, function-scoped, re-declarable)
    Var,
    /// let declaration (block-scoped, non-re-declarable)
    Let,
    /// const declaration (block-scoped, non-re-declarable, immutable)
    Const,
    /// Annex B block-level function declaration lexical binding (sloppy mode).
    BlockScopedFunction,
    /// Function parameter binding.
    Parameter,
    /// Simple catch parameter (BindingIdentifier). Per B.3.5, var declarations
    /// with the same name are allowed and reuse the catch parameter's local.
    CatchParameter,
}

impl VariableKind {
    /// Check if this variable can be reassigned
    pub fn is_const(&self) -> bool {
        matches!(self, Self::Const)
    }
}

/// A variable binding
#[derive(Debug, Clone)]
pub struct Binding {
    /// Local variable index
    pub index: u16,
    /// Variable kind
    pub kind: VariableKind,
    /// Is this captured by a closure
    pub is_captured: bool,
    /// Variable name
    pub name: String,
}

/// A lexical scope
#[derive(Debug)]
pub struct Scope {
    /// Parent scope index (None for global)
    pub parent: Option<usize>,
    /// Bindings in this scope
    pub bindings: HashMap<String, Binding>,
    /// Names declared via `var` anywhere within this lexical scope.
    ///
    /// This is used to implement early errors between `var` and lexical declarations
    /// within the same block statement list (including nested blocks), similar to
    /// ECMAScript's `VarDeclaredNames`.
    pub var_declared_names: HashSet<String>,
    /// Next local index
    pub next_local: u16,
    /// Is this a function scope
    pub is_function: bool,
    /// Scope depth (0 = global)
    pub depth: usize,
}

impl Scope {
    /// Create a new scope
    pub fn new(parent: Option<usize>, is_function: bool, depth: usize) -> Self {
        Self {
            parent,
            bindings: HashMap::new(),
            var_declared_names: HashSet::new(),
            next_local: 0,
            is_function,
            depth,
        }
    }
}

/// Scope chain for variable resolution
#[derive(Debug, Default)]
pub struct ScopeChain {
    /// All scopes
    scopes: Vec<Scope>,
    /// Current scope index
    current: Option<usize>,
}

impl ScopeChain {
    /// Create a new scope chain
    pub fn new() -> Self {
        Self::default()
    }

    /// Enter a new scope
    pub fn enter(&mut self, is_function: bool) {
        let depth = self.current.map(|i| self.scopes[i].depth + 1).unwrap_or(0);
        let scope = Scope::new(self.current, is_function, depth);
        let idx = self.scopes.len();
        self.scopes.push(scope);
        self.current = Some(idx);
    }

    /// Exit current scope
    pub fn exit(&mut self) {
        if let Some(idx) = self.current {
            self.current = self.scopes[idx].parent;
        }
    }

    /// Declare a variable in current scope
    pub fn declare(&mut self, name: &str, kind: VariableKind) -> Option<u16> {
        let current_idx = self.current?;

        // Allocate local indices at the function-scope level so they remain valid
        // after exiting block scopes.
        let function_scope_idx = self.current_function_scope_index()?;

        if kind == VariableKind::Var {
            // Early error: a `var` declaration conflicts with any existing lexical binding
            // in this scope chain up to (and including) the function scope.
            // Per B.3.5: simple catch parameters (CatchParameter kind) allow `var` redeclaration.
            let mut catch_param_index = None;
            let mut scope_idx = current_idx;
            loop {
                if let Some(existing) = self.scopes[scope_idx].bindings.get(name) {
                    if existing.kind == VariableKind::CatchParameter {
                        // B.3.5: var redeclaration of simple catch parameter is allowed.
                        // The var writes to the catch parameter's local.
                        catch_param_index = Some(existing.index);
                    } else if existing.kind != VariableKind::Var
                        && existing.kind != VariableKind::BlockScopedFunction
                        && existing.kind != VariableKind::Parameter
                    {
                        return None;
                    }
                }
                if scope_idx == function_scope_idx {
                    break;
                }
                scope_idx = self.scopes[scope_idx].parent?;
            }

            // Record that this lexical scope (and all enclosing lexical scopes)
            // contain a `var` declaration of this name. This allows detecting
            // conflicts when a lexical declaration appears later in the same block.
            let mut scope_idx = current_idx;
            loop {
                self.scopes[scope_idx]
                    .var_declared_names
                    .insert(name.to_string());
                if scope_idx == function_scope_idx {
                    break;
                }
                scope_idx = self.scopes[scope_idx].parent?;
            }

            // B.3.5: If there's a catch parameter with the same name, reuse its local.
            // The var binds to the catch parameter, NOT hoisted to function scope.
            if let Some(idx) = catch_param_index {
                return Some(idx);
            }

            // Hoist the binding to the function scope.
            if let Some(existing) = self.scopes[function_scope_idx].bindings.get(name) {
                debug_assert!(
                    existing.kind == VariableKind::Var || existing.kind == VariableKind::Parameter
                );
                return Some(existing.index);
            }

            let index = self.scopes[function_scope_idx].next_local;
            self.scopes[function_scope_idx].next_local += 1;

            self.scopes[function_scope_idx].bindings.insert(
                name.to_string(),
                Binding {
                    index,
                    kind,
                    is_captured: false,
                    name: name.to_string(),
                },
            );

            return Some(index);
        }

        // Lexical declarations: check for conflicts with `var` declarations in the
        // same block statement list (including nested blocks).
        if self.scopes[current_idx].var_declared_names.contains(name) {
            return None;
        }

        // Check for redeclaration in current lexical scope.
        if let Some(existing) = self.scopes[current_idx].bindings.get(name) {
            if kind == VariableKind::BlockScopedFunction
                && existing.kind == VariableKind::BlockScopedFunction
            {
                return Some(existing.index);
            }
            return None;
        }

        let index = self.scopes[function_scope_idx].next_local;
        self.scopes[function_scope_idx].next_local += 1;

        self.scopes[current_idx].bindings.insert(
            name.to_string(),
            Binding {
                index,
                kind,
                is_captured: false,
                name: name.to_string(),
            },
        );

        Some(index)
    }

    /// Declare an Annex B synthetic var-extension binding in function scope.
    ///
    /// Unlike normal `var` declarations this does not mark `var_declared_names`
    /// on lexical scopes, because it is paired with a block-level function
    /// lexical binding in the same statement list.
    ///
    /// Returns `Some((index, is_new))` where `is_new` is true if a fresh binding was created,
    /// false if an existing Var/Parameter binding was reused (should NOT be reinitialized).
    pub fn declare_block_function_var_extension(&mut self, name: &str) -> Option<(u16, bool)> {
        let current_idx = self.current?;
        let function_scope_idx = self.current_function_scope_index()?;

        let mut scope_idx = current_idx;
        loop {
            if let Some(existing) = self.scopes[scope_idx].bindings.get(name) {
                if existing.kind != VariableKind::Var
                    && existing.kind != VariableKind::BlockScopedFunction
                    && existing.kind != VariableKind::Parameter
                {
                    return None;
                }
            }
            if scope_idx == function_scope_idx {
                break;
            }
            scope_idx = self.scopes[scope_idx].parent?;
        }

        if let Some(existing) = self.scopes[function_scope_idx].bindings.get(name) {
            if existing.kind == VariableKind::Var || existing.kind == VariableKind::Parameter {
                return Some((existing.index, false));
            }
        }

        let index = self.scopes[function_scope_idx].next_local;
        self.scopes[function_scope_idx].next_local += 1;
        self.scopes[function_scope_idx].bindings.insert(
            name.to_string(),
            Binding {
                index,
                kind: VariableKind::Var,
                is_captured: false,
                name: name.to_string(),
            },
        );
        Some((index, true))
    }

    fn current_function_scope_index(&self) -> Option<usize> {
        let mut scope_idx = self.current?;
        loop {
            let scope = &self.scopes[scope_idx];
            if scope.is_function {
                return Some(scope_idx);
            }
            scope_idx = scope.parent?;
        }
    }

    /// Check if a name is bound as a CatchParameter in any enclosing scope
    /// (up to but not including the function scope). Returns the local index if found.
    pub fn find_catch_parameter(&self, name: &str) -> Option<u16> {
        let mut scope_idx = self.current?;
        loop {
            let scope = &self.scopes[scope_idx];
            if let Some(binding) = scope.bindings.get(name) {
                if binding.kind == VariableKind::CatchParameter {
                    return Some(binding.index);
                }
            }
            if scope.is_function {
                break;
            }
            scope_idx = scope.parent?;
        }
        None
    }

    /// Resolve a variable
    pub fn resolve(&self, name: &str) -> Option<ResolvedBinding> {
        let mut scope_idx = self.current?;
        let mut depth = 0;

        loop {
            let scope = &self.scopes[scope_idx];

            if let Some(binding) = scope.bindings.get(name) {
                if depth == 0 {
                    return Some(ResolvedBinding::Local(binding.index));
                } else {
                    return Some(ResolvedBinding::Upvalue {
                        index: binding.index,
                        depth,
                    });
                }
            }

            // Check parent scope
            if let Some(parent) = scope.parent {
                if scope.is_function {
                    depth += 1;
                }
                scope_idx = parent;
            } else {
                // Not found in any scope - it's global
                return Some(ResolvedBinding::Global(name.to_string()));
            }
        }
    }

    /// Mark a binding as captured by a closure.
    /// Returns the local index if found, None if not found.
    pub fn mark_captured(&mut self, name: &str) -> Option<u16> {
        let mut scope_idx = self.current?;

        loop {
            let scope = &mut self.scopes[scope_idx];

            if let Some(binding) = scope.bindings.get_mut(name) {
                binding.is_captured = true;
                return Some(binding.index);
            }

            // Check parent scope
            scope_idx = scope.parent?;
        }
    }

    /// Get all captured bindings in the current scope (for emitting CloseUpvalue)
    pub fn captured_bindings_in_current_scope(&self) -> Vec<u16> {
        let Some(idx) = self.current else {
            return Vec::new();
        };
        self.scopes[idx]
            .bindings
            .values()
            .filter(|b| b.is_captured)
            .map(|b| b.index)
            .collect()
    }

    /// Get current scope
    pub fn current_scope(&self) -> Option<&Scope> {
        self.current.map(|i| &self.scopes[i])
    }

    /// Get current scope mutably
    pub fn current_scope_mut(&mut self) -> Option<&mut Scope> {
        self.current.map(|i| &mut self.scopes[i])
    }

    /// Whether the current lexical scope is a function scope.
    pub fn current_scope_is_function(&self) -> bool {
        self.current
            .map(|idx| self.scopes[idx].is_function)
            .unwrap_or(false)
    }

    /// Resolve a binding in the current function scope only.
    pub fn function_scope_binding(&self, name: &str) -> Option<(u16, VariableKind)> {
        let idx = self.current_function_scope_index()?;
        self.scopes[idx]
            .bindings
            .get(name)
            .map(|b| (b.index, b.kind))
    }

    /// Get number of locals in current function scope
    pub fn local_count(&self) -> u16 {
        self.current_function_scope_index()
            .map(|idx| self.scopes[idx].next_local)
            .unwrap_or(0)
    }

    /// Allocate an anonymous local variable in the current function scope.
    /// Returns the local index. Used for internal bindings like `arguments`.
    pub fn alloc_anonymous_local(&mut self) -> Option<u16> {
        let fs = self.current_function_scope_index()?;
        let idx = self.scopes[fs].next_local;
        self.scopes[fs].next_local += 1;
        Some(idx)
    }

    /// Collect parameter names from the current function scope.
    ///
    /// Returns names of all bindings with `VariableKind::Parameter`.
    /// Used by Annex B hoisting to check "F is not an element of parameterNames".
    pub fn collect_parameter_names(&self) -> HashSet<String> {
        let Some(idx) = self.current_function_scope_index() else {
            return HashSet::new();
        };
        self.scopes[idx]
            .bindings
            .values()
            .filter(|b| b.kind == VariableKind::Parameter)
            .map(|b| b.name.clone())
            .collect()
    }

    /// Collect all local names in the current function scope chain.
    pub fn collect_local_names(&self) -> Vec<String> {
        let local_count = self.local_count();
        let mut names = vec![String::new(); local_count as usize];
        for scope in &self.scopes {
            for (name, binding) in &scope.bindings {
                if (binding.index as usize) < names.len() {
                    names[binding.index as usize] = name.clone();
                }
            }
        }
        names
    }
}

/// Result of resolving a variable
#[derive(Debug, Clone)]
pub enum ResolvedBinding {
    /// Local variable
    Local(u16),
    /// Upvalue (captured from parent function)
    Upvalue {
        /// Index in parent scope
        index: u16,
        /// Function depth
        depth: usize,
    },
    /// Global variable
    Global(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_declare_and_resolve() {
        let mut chain = ScopeChain::new();
        chain.enter(true); // function scope

        chain.declare("x", VariableKind::Let);
        chain.declare("y", VariableKind::Const);

        assert!(matches!(
            chain.resolve("x"),
            Some(ResolvedBinding::Local(0))
        ));
        assert!(matches!(
            chain.resolve("y"),
            Some(ResolvedBinding::Local(1))
        ));
    }

    #[test]
    fn test_nested_scopes() {
        let mut chain = ScopeChain::new();
        chain.enter(true); // function scope
        chain.declare("x", VariableKind::Let);

        chain.enter(false); // block scope
        chain.declare("y", VariableKind::Let);

        // y is in current scope
        assert!(matches!(
            chain.resolve("y"),
            Some(ResolvedBinding::Local(1))
        ));
        // x is in parent scope but same function
        assert!(matches!(
            chain.resolve("x"),
            Some(ResolvedBinding::Local(0))
        ));

        chain.exit();

        // y is no longer accessible
        assert!(matches!(
            chain.resolve("y"),
            Some(ResolvedBinding::Global(_))
        ));
    }

    #[test]
    fn test_global_resolution() {
        let mut chain = ScopeChain::new();
        chain.enter(true);

        // Undeclared variable resolves as global
        assert!(
            matches!(chain.resolve("console"), Some(ResolvedBinding::Global(ref s)) if s == "console")
        );
    }

    #[test]
    fn test_var_is_function_scoped() {
        let mut chain = ScopeChain::new();
        chain.enter(true); // function scope

        chain.enter(false); // block scope
        assert!(chain.declare("x", VariableKind::Var).is_some());
        chain.exit();

        // `var` binding survives exiting the block (function-scoped).
        assert!(matches!(
            chain.resolve("x"),
            Some(ResolvedBinding::Local(0))
        ));
    }

    #[test]
    fn test_var_lexical_conflict_in_same_block() {
        let mut chain = ScopeChain::new();
        chain.enter(true); // function scope
        chain.enter(false); // block scope

        assert!(chain.declare("x", VariableKind::Var).is_some());
        // `let` conflicts with any `var` declared in the same block statement list.
        assert!(chain.declare("x", VariableKind::Let).is_none());
    }

    #[test]
    fn test_var_lexical_conflict_in_enclosing_block() {
        let mut chain = ScopeChain::new();
        chain.enter(true); // function scope

        chain.enter(false); // outer block
        assert!(chain.declare("x", VariableKind::Let).is_some());

        chain.enter(false); // inner block
        // `var x` conflicts with `let x` in an enclosing block.
        assert!(chain.declare("x", VariableKind::Var).is_none());
    }

    #[test]
    fn test_var_does_not_conflict_with_lexical_in_sibling_block() {
        let mut chain = ScopeChain::new();
        chain.enter(true); // function scope

        chain.enter(false); // block 1
        assert!(chain.declare("x", VariableKind::Var).is_some());
        chain.exit();

        chain.enter(false); // block 2 (sibling)
        // `let x` is allowed in a sibling block even if `var x` exists in the function.
        assert!(chain.declare("x", VariableKind::Let).is_some());
    }
}
