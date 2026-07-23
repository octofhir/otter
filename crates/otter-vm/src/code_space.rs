//! Shared registry of linked code chunks.
//!
//! Every compiled [`BytecodeModule`] an interpreter executes — entry
//! scripts, module graphs, `eval` bodies, `new Function` bodies,
//! dynamic-import fragments — links into one interpreter-wide
//! function-id space. Linking rebases the module's function ids,
//! function-id constants, module-init records, and named-property IC
//! sites by the registry's running totals, so a function value
//! (closure, class constructor, plain function id) created in one
//! chunk stays resolvable after it escapes to a frame executing a
//! different chunk.
//!
//! This is the ownership shape production engines use: a JSC
//! `JSFunction` resolves through its own `Executable` rather than an
//! ambient per-script table, so code born in `eval` outlives the eval
//! turn. Otter keeps ids dense and chunk-relative instead of holding a
//! per-value code pointer, which leaves [`crate::Frame`],
//! [`crate::closure::JsClosure`], and [`crate::Value`] layouts
//! untouched; every [`crate::ExecutionContext`] carries the registry
//! handle and resolves foreign ids through it.
//!
//! # Contents
//!
//! - [`CodeSpace`] — append-only chunk chain with monotonic function-id and
//!   IC-site bases.
//! - [`ChunkTables`] — one linked chunk's shared tables.
//! - [`ResolvedCtx`] — borrowed-or-owned context for one function id.
//!
//! # Invariants
//!
//! - Each chunk is immutable after construction. Its `next` link is published
//!   exactly once, so reads never lock and old contexts immediately see chunks
//!   linked later in the same code space.
//! - Chunks are appended with monotonically increasing `function_base` values.
//! - A linked module's `Function::id`, `Constant::FunctionId`, and
//!   `ModuleInit::function_id` are all rebased before the executable
//!   view is built, so chunk bytecode only ever materialises global
//!   ids at runtime.
//! - Registry entries hold no [`crate::ExecutionContext`] (and thus no
//!   registry handle), so linked chunks never form an `Arc` cycle.
//! - Linked chunks live for the registry's lifetime. Escaped function
//!   values may be called arbitrarily late (timers, jobs), so nothing
//!   is evicted.
//! - IC-site bases keep dense property-IC ids globally unique, so two
//!   chunks never alias one interpreter IC slot.
//!
//! # See also
//!
//! - [`crate::execution_context`]
//! - [`crate::executable`]

use std::sync::{Arc, OnceLock};

use otter_bytecode::{BytecodeModule, Constant};

use crate::ExecutionContext;
use crate::executable::ExecutableModule;
use crate::property_atom::AtomTable;

/// One linked chunk's shared tables, as stored in the registry.
#[derive(Debug, Clone)]
pub(crate) struct ChunkTables {
    pub(crate) function_base: u32,
    pub(crate) function_count: u32,
    pub(crate) module: Arc<BytecodeModule>,
    pub(crate) executable: Arc<ExecutableModule>,
    pub(crate) atoms: Arc<AtomTable>,
}

/// Append-only registry of every code chunk linked into one interpreter.
///
/// The registry is an immutable-node chain rather than a locked `Vec`. Linking
/// claims the first empty one-shot link; resolution only follows already
/// published links. This preserves late visibility for escaped functions while
/// keeping the execution path lock-free.
#[derive(Debug, Default)]
pub(crate) struct CodeSpace {
    first: OnceLock<Arc<CodeChunk>>,
}

/// One immutable registry node. `next` is the sole publication point for a
/// later chunk; installed nodes and their tables are never replaced or evicted.
#[derive(Debug)]
struct CodeChunk {
    tables: ChunkTables,
    next: OnceLock<Arc<CodeChunk>>,
}

impl CodeSpace {
    /// Rebase `module` onto this registry's id space and append it as a new
    /// immutable chunk. Returns the chunk's [`ExecutionContext`] bound to this
    /// code space.
    ///
    /// `Interpreter::link_module` is the normal single-writer entry point. The
    /// one-shot chain also makes adopting the same standalone context into two
    /// interpreters sound: concurrent linkers advance through already claimed
    /// links until one publishes its own module, with no duplicated id range.
    pub(crate) fn link_module(self: &Arc<Self>, module: BytecodeModule) -> ExecutionContext {
        let mut pending = Some(module);
        let mut next = &self.first;
        let mut function_base = 0;
        let mut property_ic_base = 0;

        loop {
            let chunk = next.get_or_init(|| {
                let mut module = pending
                    .take()
                    .expect("a code-space link publishes its module exactly once");
                rebase_module(&mut module, function_base);
                let function_count = u32::try_from(module.functions.len())
                    .expect("chunk function table exceeds u32 range");
                let tables = ChunkTables {
                    function_base,
                    function_count,
                    executable: Arc::new(ExecutableModule::from_bytecode_with_ic_base(
                        &module,
                        property_ic_base,
                    )),
                    atoms: Arc::new(AtomTable::from_constants(&module.constants)),
                    module: Arc::new(module),
                };
                Arc::new(CodeChunk {
                    tables,
                    next: OnceLock::new(),
                })
            });

            if pending.is_none() {
                return ExecutionContext::from_chunk_tables(chunk.tables.clone(), Arc::clone(self));
            }

            function_base = chunk
                .tables
                .function_base
                .checked_add(chunk.tables.function_count)
                .expect("code space exhausted the u32 function-id range");
            property_ic_base = chunk.tables.executable.property_ic_site_end();
            next = &chunk.next;
        }
    }

    /// Resolve the chunk owning `function_id`, if any chunk was linked
    /// over that id.
    pub(crate) fn chunk_for(&self, function_id: u32) -> Option<&ChunkTables> {
        let mut chunk = self.first.get();
        while let Some(current) = chunk {
            let tables = &current.tables;
            if function_id < tables.function_base {
                return None;
            }
            if function_id - tables.function_base < tables.function_count {
                return Some(tables);
            }
            chunk = current.next.get();
        }
        None
    }

    /// Read one function's material-feedback epoch without requiring an
    /// ambient execution context. Used only by the explicit optimizing-tier
    /// policy query; baseline compilation and dispatch do not call it.
    pub(crate) fn feedback_epoch(&self, function_id: u32) -> Option<u32> {
        let chunk = self.chunk_for(function_id)?;
        chunk
            .executable
            .function(function_id - chunk.function_base)
            .map(crate::executable::CodeBlock::feedback_epoch)
    }
}

/// A chunk resolved for one function id: either the caller's ambient
/// context (borrowed, the hot in-chunk path) or a context rebuilt from
/// a foreign registry chunk (owned, a few `Arc` clones).
#[derive(Debug)]
pub(crate) enum ResolvedCtx<'a> {
    Ambient(&'a ExecutionContext),
    Owned(ExecutionContext),
}

impl std::ops::Deref for ResolvedCtx<'_> {
    type Target = ExecutionContext;

    fn deref(&self) -> &ExecutionContext {
        match self {
            Self::Ambient(context) => context,
            Self::Owned(context) => context,
        }
    }
}

/// Shift every function-id-bearing record in `module` by `base` so the
/// chunk's ids are unique within the owning [`CodeSpace`].
fn rebase_module(module: &mut BytecodeModule, base: u32) {
    if base == 0 {
        return;
    }
    for function in &mut module.functions {
        function.id = function
            .id
            .checked_add(base)
            .expect("rebased function id exceeds u32 range");
        for site in &mut function.class_hint_sites {
            site.class_function_id = site
                .class_function_id
                .checked_add(base)
                .expect("rebased class-hint function id exceeds u32 range");
        }
    }
    for constant in &mut module.constants {
        if let Constant::FunctionId { index } = constant {
            *index = index
                .checked_add(base)
                .expect("rebased function-id constant exceeds u32 range");
        }
    }
    for init in &mut module.module_inits {
        init.function_id = init
            .function_id
            .checked_add(base)
            .expect("rebased module-init function id exceeds u32 range");
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use otter_bytecode::{
        BytecodeModule, Constant, Function, Instruction, ModuleInit, Op, Operand, SourceKind,
    };

    use super::CodeSpace;

    fn module_with_functions(count: u32) -> BytecodeModule {
        let functions = (0..count)
            .map(|id| Function {
                id,
                name: format!("f{id}"),
                code: vec![Instruction {
                    pc: 0,
                    op: Op::ReturnUndefined,
                    operands: Vec::new(),
                }]
                .into(),
                ..Function::default()
            })
            .collect();
        BytecodeModule {
            module: "<test>".to_string(),
            template_sites: Vec::new(),
            source_kind: SourceKind::JavaScript,
            functions,
            constants: vec![Constant::FunctionId { index: 1 }],
            module_resolutions: Vec::new(),
            module_inits: vec![ModuleInit {
                url: "test:mod".to_string(),
                function_id: 1,
            }],
        }
    }

    #[test]
    fn first_chunk_links_at_base_zero_unrebased() {
        let space = Arc::new(CodeSpace::default());
        let context = space.link_module(module_with_functions(3));
        assert_eq!(context.function_base(), 0);
        assert_eq!(context.function_id_constant(0), Some(1));
        assert!(context.exec_function(0).is_some());
        assert!(context.exec_function(2).is_some());
        assert!(context.exec_function(3).is_none());
    }

    #[test]
    fn second_chunk_rebases_ids_constants_and_inits() {
        let space = Arc::new(CodeSpace::default());
        let _first = space.link_module(module_with_functions(3));
        let second = space.link_module(module_with_functions(2));
        assert_eq!(second.function_base(), 3);
        assert_eq!(second.function_id_constant(0), Some(4));
        assert_eq!(second.module_init_function_id("test:mod"), Some(4));
        assert!(second.exec_function(3).is_some());
        assert!(second.exec_function(4).is_some());
        assert!(
            second.exec_function(2).is_some(),
            "sibling-chunk ids resolve transparently through the shared space",
        );
        assert!(second.exec_function(5).is_none());
    }

    #[test]
    fn foreign_ids_resolve_through_any_linked_context() {
        let space = Arc::new(CodeSpace::default());
        let first = space.link_module(module_with_functions(3));
        let second = space.link_module(module_with_functions(2));
        let foreign = first.for_function(4).expect("second chunk's id resolves");
        assert_eq!(foreign.function_base(), 3);
        assert!(foreign.exec_function(4).is_some());
        assert_eq!(
            foreign.function(4).map(|f| f.name.as_str()),
            Some("f1"),
            "global id 4 is the second chunk's local function 1",
        );
        let back = second.for_function(0).expect("first chunk's id resolves");
        assert_eq!(back.function_base(), 0);
        assert!(first.for_function(5).is_none());
    }

    #[test]
    fn ic_sites_continue_across_chunks() {
        let space = Arc::new(CodeSpace::default());
        let mut module = module_with_functions(1);
        module.functions[0].code = vec![
            Instruction {
                pc: 0,
                op: Op::LoadProperty,
                operands: vec![
                    Operand::Register(0),
                    Operand::Register(0),
                    Operand::ConstIndex(0),
                ],
            },
            Instruction {
                pc: 1,
                op: Op::ReturnUndefined,
                operands: Vec::new(),
            },
        ]
        .into();
        module.constants = vec![Constant::String {
            utf16: "x".encode_utf16().collect(),
        }];
        module.module_inits.clear();
        let second_module = module.clone();
        let first = space.link_module(module);
        let second = space.link_module(second_module);
        assert_eq!(first.property_ic_site_end(), 1);
        assert_eq!(second.property_ic_site_end(), 2);
        assert_eq!(first.property_ic_site(0, 0), Some(0));
        assert_eq!(second.property_ic_site(1, 0), Some(1));
    }

    #[test]
    fn concurrent_linkers_claim_disjoint_function_ranges() {
        let space = Arc::new(CodeSpace::default());
        let mut joins = Vec::new();
        for _ in 0..4 {
            let space = Arc::clone(&space);
            joins.push(std::thread::spawn(move || {
                space.link_module(module_with_functions(2)).function_base()
            }));
        }

        let mut bases: Vec<_> = joins
            .into_iter()
            .map(|join| join.join().expect("code-space linker completes"))
            .collect();
        bases.sort_unstable();
        assert_eq!(bases, [0, 2, 4, 6]);
        for function_id in 0..8 {
            assert!(space.chunk_for(function_id).is_some());
        }
    }

    #[test]
    fn immutable_published_nodes_keep_code_space_and_context_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}

        assert_send_sync::<CodeSpace>();
        assert_send_sync::<crate::ExecutionContext>();
    }
}
