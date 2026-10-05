//! Compiler stack and static name resolution across frames and contexts.
//!
//! The compile stack mirrors lexical function nesting: a child function is
//! compiled at its creation site, so every enclosing frame's scope stack is
//! frozen exactly as the child's closure context will see it. Resolution
//! walks that static chain innermost-first — the current frame's scopes, each
//! enclosing frame's scopes, then (for a direct-eval body) the caller's
//! context chain — counting one hop per scope that owns a context.
//!
//! # Contents
//! - [`Compiler`] — the frame stack plus compilation-unit state.
//! - [`ScopeLocation`] / [`Resolved`] / [`NameRef`] — a resolved binding and
//!   how to reach it (own register or slot, a slot `depth` hops out of the
//!   closure context, or the global environment), plus the eval-extension
//!   probe range when a sloppy direct eval may shadow it.
//! - [`VarTarget`] — where a sloppy eval or Annex B function sync writes its
//!   variable-scope binding.
//! - module-scope slot names ([`MODULE_ENV_BINDING`] and friends).
//!
//! # Invariants
//! - The stack is never empty while lowering.
//! - A depth counts only scopes that own a context; a function without
//!   contexts is transparent, and its closure context is its creator's.
//! - `Lookup*` operations are emitted exactly when a context carrying an
//!   eval extension lies strictly between the reference and its target
//!   (or anywhere on the chain for a global reference).
//! - Outer-frame register bindings are never reached: capture analysis
//!   promotes every name a nested function references to a slot.
//!
//! # See also
//! - `binding_emit` for the bytecode each resolution lowers to.
//! - `function_context` for per-frame scopes and contexts.
//! - `capture` for the capture pre-pass.

use crate::scope::CtxReg;
use crate::*;
use otter_bytecode::{EvalCallerChain, SlotKind};

/// Compile-time stack of function contexts. The innermost context
/// is at the top.
///
/// The compiler exposes the inner-most [`FunctionContext`] through
/// `Deref` / `DerefMut` so code uses `cx.emit`, `cx.scratch`, etc.
/// without referencing the stack explicitly.
#[derive(Debug)]
pub(crate) struct Compiler {
    pub(crate) stack: Vec<FunctionContext>,
    /// The caller context chain of a direct-eval body (frame 0 is the eval
    /// `<main>`); `None` for scripts, modules, and indirect eval.
    pub(crate) eval_chain: Option<EvalCallerChain>,
    /// One-shot hint set by class-constructor lowering: the next
    /// `compile_function_full` frame carries a [[HomeObject]] and (when
    /// paired with `next_fn_derived_ctor`) is a derived constructor.
    pub(crate) next_fn_has_home: bool,
    pub(crate) next_fn_derived_ctor: bool,
    /// One-shot: the next NAMED FunctionExpression lowering skips its
    /// §10.2.11 self-name funcEnv binding — §20.2.1.1
    /// CreateDynamicFunction's `function anonymous(...)` binds nothing.
    pub(crate) next_fn_expr_no_self_binding: bool,
    /// One-shot hint set by MethodDefinition lowering: the next
    /// `compile_function_full` marks its record `is_method`.
    pub(crate) next_fn_is_method: bool,
    /// One-shot hint paired with [`Self::next_fn_is_method`]: the
    /// next `compile_function_full` resolves `super.x` through the
    /// statics-side home object.
    pub(crate) next_fn_static_home: bool,
    /// One-shot byte range for the next compiled function's §20.2.3.5
    /// [[SourceText]] when it differs from the function-body span.
    pub(crate) next_fn_source_text_span: Option<(u32, u32)>,
    /// Stack of private-name class levels — one per enclosing class
    /// declaration, innermost last. The value is a per-unit class id.
    pub(crate) private_namespaces: Vec<u32>,
    /// Private names declared by each enclosing class, parallel to
    /// `private_namespaces`.
    pub(crate) class_private_names: Vec<std::collections::HashSet<String>>,
    /// Instance private METHOD / ACCESSOR names per enclosing class
    /// (parallel to `private_namespaces`). Access to these emits a
    /// §7.3.31 brand check.
    pub(crate) class_private_instance_methods: Vec<std::collections::HashSet<String>>,
    /// Scope of each enclosing class (parallel to `private_namespaces`):
    /// its private names, brand, and homes are slots of that scope.
    pub(crate) class_scope_locations: Vec<ScopeLocation>,
    /// `true` when compiling any `eval` body — §B.3.3.3 makes the
    /// Annex B global function extension *deletable* for eval code.
    pub(crate) in_eval: bool,
    /// `true` when compiling a *strict* `eval` body or a sloppy direct
    /// eval whose variable environment is a function's: top-level `var` /
    /// `function` declarations do not mirror onto the global object.
    pub(crate) suppress_global_mirror: bool,
    /// §16.1.7 — names of script top-level `var` and function
    /// declarations, which live as global-object properties. Empty for
    /// modules and function-caller eval bodies.
    pub(crate) script_global_vars: std::collections::HashSet<String>,
    /// §9.1.1.4 global declarative record — names of script
    /// top-level `let` / `const` / `class` declarations.
    pub(crate) script_global_lexicals: std::collections::HashSet<String>,
    /// `true` while lowering class instance-field initializers
    /// (which compile into the constructor frame).
    pub(crate) in_field_initializer: bool,
    /// `true` when this eval body's caller permits `new.target`.
    pub(crate) eval_new_target_allowed: bool,
    /// Memoized `expr_number_typed` results, keyed by AST-node address.
    pub(crate) number_typed_cache: RefCell<HashMap<usize, bool>>,
    /// Interned annotation names referenced by [`TypeHint::Class`].
    pub(crate) class_hint_names: Vec<String>,
    /// Reverse index into [`Self::class_hint_names`].
    pub(crate) class_hint_name_ids: HashMap<String, u32>,
    /// Module-wide `class` declarations: name → constructor function id, or
    /// `None` once a second class of the same name is seen.
    pub(crate) declared_classes: HashMap<String, Option<u32>>,
    /// Class-annotated property sites awaiting name resolution.
    pub(crate) pending_class_hint_sites: Vec<PendingClassHintSite>,
    /// Capture facts of the unit being compiled.
    pub(crate) capture: Rc<crate::capture::CaptureFacts>,
}

/// One class-annotated property site, still holding the interned annotation
/// name rather than a resolved class.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PendingClassHintSite {
    pub(crate) function_id: u32,
    pub(crate) pc: u32,
    pub(crate) name: u32,
}

/// Position of a scope on the static chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScopeLocation {
    /// Scope `scope` of compile frame `frame`.
    Frame { frame: usize, scope: usize },
    /// Entry `hop` of the direct-eval caller chain.
    Chain { hop: usize },
}

/// Probe range of an eval-extension lookup: `depth` hops from `base`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LookupSite {
    pub(crate) base: CtxReg,
    pub(crate) depth: u16,
}

/// How a resolved binding is reached from the current frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    /// A binding of the current frame; `info.storage` addresses it.
    Own,
    /// Slot `slot` of the context `depth` hops out of the closure context.
    Outer { depth: u16, slot: u16 },
}

/// A statically resolved binding.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Resolved {
    pub(crate) location: ScopeLocation,
    pub(crate) info: BindingInfo,
    pub(crate) access: Access,
    /// Extension probe needed before the binding: a sloppy direct eval may
    /// have created a shadowing `var` between here and the binding.
    pub(crate) lookup: Option<LookupSite>,
}

impl Resolved {
    /// Slot index of a context-slot binding.
    pub(crate) fn slot(&self) -> Option<u16> {
        match (self.access, self.info.storage) {
            (Access::Outer { slot, .. }, _) => Some(slot),
            (Access::Own, BindingStorage::Slot { slot, .. }) => Some(slot),
            (Access::Own, BindingStorage::Register { .. }) => None,
        }
    }
}

/// A name reference: a static binding or the global environment.
#[derive(Debug, Clone, Copy)]
pub(crate) enum NameRef {
    Binding(Resolved),
    /// No static binding; the extension probe range when one is needed.
    Global(Option<LookupSite>),
}

impl NameRef {
    pub(crate) fn lookup(&self) -> Option<LookupSite> {
        match self {
            Self::Binding(resolved) => resolved.lookup,
            Self::Global(lookup) => *lookup,
        }
    }
}

/// Where a variable-scope binding (sloppy eval `var`, Annex B function
/// sync) is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VarTarget {
    /// A binding of the current frame.
    Own(BindingStorage),
    /// A caller variable-scope slot `depth` hops out of the closure context.
    Outer { depth: u16, slot: u16 },
    /// The eval extension of the caller's variable scope, `depth` hops out
    /// of the closure context.
    Extension { depth: u16 },
}

/// One scope visited by [`Compiler::walk_chain`].
#[derive(Debug, Clone, Copy)]
struct ChainEntry {
    location: ScopeLocation,
    /// Hop from the walk base when the scope owns a context.
    hop: Option<u16>,
    extension: bool,
}

/// Walk state handed to each visited scope.
#[derive(Debug, Clone, Copy)]
struct WalkState {
    base: CtxReg,
    /// Hop of the current frame's closure context from the base; known once
    /// the current frame's scopes are walked.
    closure_hop: u16,
    /// Hop of the innermost extension-capable context seen so far.
    first_extension: Option<u16>,
}

impl Compiler {
    pub(crate) fn new(top: FunctionContext, capture: crate::capture::CaptureFacts) -> Self {
        Self {
            stack: vec![top],
            eval_chain: None,
            next_fn_has_home: false,
            next_fn_derived_ctor: false,
            next_fn_expr_no_self_binding: false,
            next_fn_is_method: false,
            next_fn_static_home: false,
            next_fn_source_text_span: None,
            private_namespaces: Vec::new(),
            class_private_names: Vec::new(),
            class_private_instance_methods: Vec::new(),
            class_scope_locations: Vec::new(),
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
            capture: Rc::new(capture),
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
    /// module-wide list.
    pub(crate) fn take_class_hint_sites(&mut self, function_id: u32, sites: Vec<(u32, u32)>) {
        self.pending_class_hint_sites
            .extend(sites.into_iter().map(|(pc, name)| PendingClassHintSite {
                function_id,
                pc,
                name,
            }));
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

    /// Location of scope `scope` in the current frame.
    pub(crate) fn here(&self, scope: usize) -> ScopeLocation {
        ScopeLocation::Frame {
            frame: self.stack.len() - 1,
            scope,
        }
    }

    /// Location of the innermost scope of the current frame.
    pub(crate) fn innermost_location(&self) -> ScopeLocation {
        self.here(self.scopes.len().saturating_sub(1))
    }

    /// Walk the static chain innermost-first — the current frame's scopes,
    /// each enclosing frame's scopes, then the eval caller chain — until
    /// `visit` returns `Some`. `visit` sees each scope before it is counted
    /// as an extension, so `first_extension` covers only inner scopes.
    fn walk_chain<T>(
        &self,
        mut visit: impl FnMut(&ChainEntry, &WalkState) -> Option<T>,
    ) -> Option<T> {
        let top = self.stack.len() - 1;
        let mut state = WalkState {
            base: self.stack[top].innermost_ctx(),
            closure_hop: 0,
            first_extension: None,
        };
        let mut hop: u32 = 0;
        for frame_index in (0..=top).rev() {
            let frame = &self.stack[frame_index];
            for scope_index in (0..frame.scopes.len()).rev() {
                let scope = &frame.scopes[scope_index];
                let this_hop = scope
                    .context
                    .map(|_| u16::try_from(hop).unwrap_or(u16::MAX));
                let entry = ChainEntry {
                    location: ScopeLocation::Frame {
                        frame: frame_index,
                        scope: scope_index,
                    },
                    hop: this_hop,
                    extension: this_hop.is_some() && scope.flags.has_extension,
                };
                if let Some(found) = visit(&entry, &state) {
                    return Some(found);
                }
                if this_hop.is_some() {
                    hop += 1;
                }
                if entry.extension && state.first_extension.is_none() {
                    state.first_extension = entry.hop;
                }
            }
            if frame_index == top {
                state.closure_hop = u16::try_from(hop).unwrap_or(u16::MAX);
            }
        }
        if let Some(chain) = &self.eval_chain {
            for (index, scope) in chain.scopes.iter().enumerate() {
                let entry = ChainEntry {
                    location: ScopeLocation::Chain { hop: index },
                    hop: Some(u16::try_from(hop + index as u32).unwrap_or(u16::MAX)),
                    extension: scope.descriptor.flags.has_extension,
                };
                if let Some(found) = visit(&entry, &state) {
                    return Some(found);
                }
                if entry.extension && state.first_extension.is_none() {
                    state.first_extension = entry.hop;
                }
            }
        }
        None
    }

    /// Ordering rank of a location: a larger rank is lexically inner.
    pub(crate) fn location_rank(&self, location: ScopeLocation) -> (usize, usize) {
        match location {
            ScopeLocation::Frame { frame, scope } => (frame + 1, scope + 1),
            ScopeLocation::Chain { hop } => {
                let len = self.eval_chain.as_ref().map_or(0, |c| c.scopes.len());
                (0, len.saturating_sub(hop))
            }
        }
    }

    /// Binding record of `name` directly in the scope at `location`.
    fn binding_at(&self, location: ScopeLocation, name: &str) -> Option<BindingInfo> {
        match location {
            ScopeLocation::Frame { frame, scope } => {
                self.stack[frame].scopes[scope].bindings.get(name).copied()
            }
            ScopeLocation::Chain { hop } => {
                let descriptor = &self.eval_chain.as_ref()?.scopes.get(hop)?.descriptor;
                let slot = descriptor.slots.iter().position(|slot| slot.name == name)?;
                let kind = descriptor.slots[slot].kind;
                let mut info = BindingInfo::new(
                    BindingStorage::Slot {
                        ctx: CtxReg::Closure,
                        slot: slot as u16,
                    },
                    kind,
                );
                info.initialized = true;
                info.fn_self_name = matches!(kind, SlotKind::FnSelfName | SlotKind::Class);
                Some(info)
            }
        }
    }

    fn resolve_entry(
        &self,
        entry: &ChainEntry,
        state: &WalkState,
        info: BindingInfo,
    ) -> Option<Resolved> {
        let top = self.stack.len() - 1;
        let own = matches!(entry.location, ScopeLocation::Frame { frame, .. } if frame == top);
        let access = if own {
            Access::Own
        } else {
            let BindingStorage::Slot { slot, .. } = info.storage else {
                return None;
            };
            let hop = entry.hop?;
            Access::Outer {
                depth: hop.checked_sub(state.closure_hop)?,
                slot,
            }
        };
        let lookup = match (entry.hop, info.storage) {
            (Some(target), BindingStorage::Slot { .. })
                if state.first_extension.is_some_and(|hop| hop < target) =>
            {
                Some(LookupSite {
                    base: state.base,
                    depth: target,
                })
            }
            _ => None,
        };
        Some(Resolved {
            location: entry.location,
            info,
            access,
            lookup,
        })
    }

    /// Resolve `name` to its innermost static binding, if any. An
    /// outer-frame register binding (never reachable) is skipped.
    pub(crate) fn resolve_name(&self, name: &str) -> Option<Resolved> {
        self.walk_chain(|entry, state| {
            let info = self.binding_at(entry.location, name)?;
            self.resolve_entry(entry, state, info)
        })
    }

    /// Resolve `name` as declared directly in the scope at `location`.
    pub(crate) fn resolve_at(&self, location: ScopeLocation, name: &str) -> Option<Resolved> {
        self.walk_chain(|entry, state| {
            if entry.location != location {
                return None;
            }
            let info = self.binding_at(location, name)?;
            self.resolve_entry(entry, state, info)
        })
    }

    /// Extension probe range for a reference with no static binding.
    pub(crate) fn global_lookup(&self) -> Option<LookupSite> {
        let mut outermost: Option<u16> = None;
        let mut base = CtxReg::Closure;
        self.walk_chain::<()>(|entry, state| {
            base = state.base;
            if entry.extension {
                outermost = entry.hop;
            }
            None
        });
        Some(LookupSite {
            base,
            depth: outermost?.saturating_add(1),
        })
    }

    /// Resolve `name` for a load / store / `typeof` / `delete`.
    pub(crate) fn resolve_ref(&self, name: &str) -> NameRef {
        match self.resolve_name(name) {
            Some(resolved) => NameRef::Binding(resolved),
            None => NameRef::Global(self.global_lookup()),
        }
    }

    /// Rank of the innermost static binding of `name`, for ordering it
    /// against `with` object environments (§9.1.1.2.1).
    pub(crate) fn binding_position(&self, name: &str) -> Option<(usize, usize)> {
        self.resolve_name(name)
            .map(|resolved| self.location_rank(resolved.location))
    }

    /// `true` when `name` has no static binding and no eval extension or
    /// `with` object can intercept it, so it reads the global object.
    pub(crate) fn resolves_to_plain_global(&self, name: &str) -> bool {
        self.active_with_envs.is_empty()
            && self.resolve_name(name).is_none()
            && self.global_lookup().is_none()
    }
}

/// Module-scope slot holding the module environment object.
pub(crate) const MODULE_ENV_BINDING: &str = "%module_env";
/// Module-scope slot holding the `import.meta` object.
pub(crate) const IMPORT_META_BINDING: &str = "%import_meta";

/// Module-scope slot holding the import record of request number `index`.
pub(crate) fn import_record_binding(index: u16) -> String {
    format!("%import_record_{index}")
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
