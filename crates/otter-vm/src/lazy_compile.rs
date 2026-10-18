//! First-call compile of lazily compiled function bodies.
//!
//! A chunk can carry lazy records ([`otter_bytecode::LazyFunction`]): a
//! function's call-visible shape with a placeholder body. The first
//! activation of one stops before its body runs; the host's
//! [`LazyCompileHook`] compiles the body against the closure's context
//! chain, the body links as a chunk of its own whose function 0 answers to
//! the record's id, and the creating chunk forwards the id there. The
//! activation then re-enters as a tail call, so the frame the body runs in
//! has the body's own layout.
//!
//! # Contents
//! - [`LazyCompileInput`] / [`LazyCompileHook`] — the host compile.
//! - `Interpreter::compile_lazy_body` — compile (or take the body a code
//!   cache kept, [`crate::lazy_body_cache`]), link and forward one record.
//!
//! # Invariants
//! - A record compiles at most once; afterwards every resolution of its id
//!   reaches the compiled body (`ExecutionContext::for_function`).
//! - The record's entry cell takes the body's register window and call
//!   flags in place, before any frame of the body is built.
//! - The body chunk shares the creating chunk's source registry: its spans
//!   and [[SourceText]] ranges index the same text.
//!
//! # See also
//! - `crate::code_space::CodeSpace::link_lazy_body`
//! - `crate::interp::call_dispatch` for the first-entry interception.

use otter_bytecode::{
    BytecodeModule, EvalCallerChain, Function, LazyEnvironment, ModuleResolution,
};

use crate::{ExecutionContext, Interpreter, Value, VmError};

/// What the host compiles a lazy body from.
#[derive(Debug, Clone, Copy)]
pub struct LazyCompileInput<'a> {
    /// Whole source text of the module the function belongs to.
    pub source: &'a str,
    /// URL of that module: the record's own, or its chunk's when the record
    /// names none.
    pub module_url: &'a str,
    /// The function's lazy record.
    pub function: &'a Function,
    /// The closure's context chain, innermost first.
    pub chain: &'a EvalCallerChain,
    /// Top-level facts of the function's module, when it has any.
    pub environment: Option<&'a LazyEnvironment>,
    /// Import resolutions of the chunk holding the record.
    pub resolutions: &'a [ModuleResolution],
}

/// Host compile of one lazy body; `Err` carries the SyntaxError message.
pub type LazyCompileHook =
    std::sync::Arc<dyn Fn(LazyCompileInput<'_>) -> Result<BytecodeModule, String> + Send + Sync>;

impl Interpreter {
    /// Install the compiler lazy records' bodies are compiled with.
    pub fn set_lazy_compile_hook(&mut self, hook: Option<LazyCompileHook>) {
        self.lazy_compile_hook = hook;
    }

    /// How many lazy records' bodies this isolate has compiled.
    #[must_use]
    pub fn lazy_bodies_compiled(&self) -> u64 {
        self.lazy_bodies_compiled
    }

    /// Compile the body of lazy record `function_id`, created over the
    /// context `closure_context`, link it and forward the record to it.
    ///
    /// # Errors
    /// A SyntaxError when the host cannot compile the body; a link failure
    /// as its VM error.
    pub(crate) fn compile_lazy_body(
        &mut self,
        context: &ExecutionContext,
        function_id: u32,
        closure_context: Value,
    ) -> Result<(), VmError> {
        let hook = self
            .lazy_compile_hook
            .clone()
            .ok_or_else(|| self.err_syntax("lazy function body without a compiler".into()))?;
        let record = context
            .function(function_id)
            .ok_or(VmError::InvalidOperand)?;
        let chain = self.eval_caller_chain(context, closure_context)?;
        let module = context.linked_module();
        let url = if record.module_url.is_empty() {
            context.module_name()
        } else {
            record.module_url.as_str()
        };
        let source = match context.sources().get(url) {
            Some(source) => source.text(),
            None => module.function_source.as_deref().ok_or_else(|| {
                self.err_syntax(
                    format!(
                        "no source for the body of `{}` ({url}, chunk {})",
                        record.name,
                        context.module_name()
                    )
                    .into(),
                )
            })?,
        };
        // A script's records take its URL only when it links; its
        // environment keeps the name it was compiled under.
        let environment = module
            .lazy_environments
            .iter()
            .find(|environment| environment.module_url == record.module_url)
            .or(match module.lazy_environments.as_slice() {
                [only] => Some(only),
                _ => None,
            });
        // A script the embedder caches keeps each first-call body, and a
        // body it kept on an earlier load answers this call.
        let kept = record
            .lazy
            .filter(|_| self.lazy_body_store.keeps(url))
            .map(|lazy| {
                let fingerprint = crate::lazy_body_cache::fingerprint(
                    &chain,
                    environment,
                    &module.module_resolutions,
                    url,
                );
                (lazy.range, fingerprint)
            });
        let reused = kept.and_then(|(range, fingerprint)| {
            let bytes = self.lazy_body_store.find(url, range, fingerprint)?;
            crate::lazy_body_cache::decode(bytes, url)
        });
        let body = match reused {
            Some(body) => body,
            None => {
                let body = hook(LazyCompileInput {
                    source,
                    module_url: url,
                    function: record,
                    chain: &chain,
                    environment,
                    resolutions: &module.module_resolutions,
                })
                .map_err(|message| self.err_syntax(message.into()))?;
                if let Some((range, fingerprint)) = kept
                    && let Some(bytes) = crate::lazy_body_cache::encode(&body)
                {
                    self.lazy_body_store.record(
                        url,
                        crate::lazy_body_cache::LazyBody {
                            range,
                            fingerprint,
                            bytes,
                        },
                    );
                }
                self.lazy_bodies_compiled += 1;
                body
            }
        };
        let realm_id = self.function_realm_id(function_id);
        let body_context = std::sync::Arc::clone(&self.code_space)
            .link_lazy_body(
                body,
                context.sources().clone(),
                &self.resource_account,
                function_id,
            )
            .map_err(|_| VmError::InvalidOperand)?;
        body_context.resolve_atoms(self.shape_runtime.names_mut());
        let body_base = body_context.function_base();
        let linked = body_context.linked_functions();
        for function in linked.iter().skip(1) {
            self.jit_code_registry.link_function(function, realm_id);
            if realm_id != 0 {
                self.function_realm_ids.insert(function.id, realm_id);
            }
        }
        self.jit_code_registry
            .relink_lazy_body(function_id, &linked[0]);
        context.set_lazy_forward(function_id, body_base);
        self.owner_cache.borrow_mut().forget(function_id);
        Ok(())
    }
}
