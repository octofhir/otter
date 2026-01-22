//! Scope management for variable resolution

use std::collections::HashMap;

/// A variable binding
#[derive(Debug, Clone)]
pub struct Binding {
    /// Local variable index
    pub index: u16,
    /// Is this a const binding
    pub is_const: bool,
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
    pub fn declare(&mut self, name: &str, is_const: bool) -> Option<u16> {
        let scope = self.current_scope_mut()?;

        // Check for redeclaration
        if scope.bindings.contains_key(name) {
            return None; // Already declared
        }

        let index = scope.next_local;
        scope.next_local += 1;

        scope.bindings.insert(
            name.to_string(),
            Binding {
                index,
                is_const,
                is_captured: false,
                name: name.to_string(),
            },
        );

        Some(index)
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

    /// Get current scope
    pub fn current_scope(&self) -> Option<&Scope> {
        self.current.map(|i| &self.scopes[i])
    }

    /// Get current scope mutably
    pub fn current_scope_mut(&mut self) -> Option<&mut Scope> {
        self.current.map(|i| &mut self.scopes[i])
    }

    /// Get number of locals in current function scope
    pub fn local_count(&self) -> u16 {
        let mut scope_idx = self.current;
        let mut count = 0u16;

        while let Some(idx) = scope_idx {
            let scope = &self.scopes[idx];
            count = count.saturating_add(scope.next_local);

            if scope.is_function {
                break;
            }
            scope_idx = scope.parent;
        }

        count
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

        chain.declare("x", false);
        chain.declare("y", true);

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
        chain.declare("x", false);

        chain.enter(false); // block scope
        chain.declare("y", false);

        // y is in current scope
        assert!(matches!(
            chain.resolve("y"),
            Some(ResolvedBinding::Local(0))
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
}
