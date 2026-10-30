//! Linked `new Function` sources, reused by later calls with the same text.
//!
//! CreateDynamicFunction (§20.2.1.1.1) compiles its synthesized source on
//! every call. Running the `<main>` of an already linked chunk again creates
//! a fresh function object — its own prototype, closure state and identity —
//! over the same code, so repeated `new Function(params, body)` calls with
//! one text compile and link once.
//!
//! # Contents
//! - [`DynamicFunctionSource`]: the key — kind, realm, parameter text, body.
//! - [`DynamicFunctionCache`]: key → base of the chunk that source linked as.
//!
//! # Invariants
//! - An entry names its chunk by function base only and never retains it, so
//!   code eviction still reclaims the chunk once nothing runs it. Bases are
//!   never reused: a lookup that finds the chunk gone drops the entry.
//! - Only successful links are entered, under the exact parameter and body
//!   text whose separate probes passed.
//! - A source containing a template literal is never entered: each
//!   evaluation of a dynamic function's source is a new Parse Node, so its
//!   template objects must be fresh (§13.2.8.4 GetTemplateObject).
//! - The cache is bounded; reaching the bound empties it.
//!
//! # See also
//! - `Interpreter::build_dynamic_function_from_parts` in [`crate::eval_ops`].

use std::sync::Arc;

use crate::code_space::{ChunkResolution, CodeSpace};
use crate::eval_ops::DynamicFunctionKind;
use crate::execution_context::ExecutionContext;

/// Entries kept before the cache empties.
const CAPACITY: usize = 256;

/// The complete input of one CreateDynamicFunction compile.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct DynamicFunctionSource {
    pub(crate) kind: DynamicFunctionKind,
    /// Function ids are realm-bound when linked.
    pub(crate) realm: u32,
    /// Parameter strings joined with `,`, as the source assembles them.
    pub(crate) params: String,
    pub(crate) body: String,
}

impl DynamicFunctionSource {
    /// `false` when a template literal may occur in the source. A template
    /// literal needs a backtick in the text, so its absence is proof.
    pub(crate) fn reusable(&self) -> bool {
        !self.params.contains('`') && !self.body.contains('`')
    }
}

/// Chunks linked from dynamic function sources, by source.
#[derive(Debug, Default)]
pub(crate) struct DynamicFunctionCache {
    chunks: rustc_hash::FxHashMap<DynamicFunctionSource, u32>,
}

impl DynamicFunctionCache {
    /// The live chunk `source` linked as, if one is cached.
    pub(crate) fn lookup(
        &mut self,
        source: &DynamicFunctionSource,
        space: &Arc<CodeSpace>,
    ) -> Option<ExecutionContext> {
        let base = *self.chunks.get(source)?;
        match space.resolve_chunk(base) {
            ChunkResolution::Live {
                function_base,
                payload,
            } if function_base == base => Some(ExecutionContext::from_chunk_payload(
                payload,
                base,
                Arc::clone(space),
            )),
            _ => {
                self.chunks.remove(source);
                None
            }
        }
    }

    /// Record that `source` linked as the chunk at `function_base`.
    pub(crate) fn insert(&mut self, source: DynamicFunctionSource, function_base: u32) {
        if !source.reusable() {
            return;
        }
        if self.chunks.len() >= CAPACITY {
            self.chunks.clear();
        }
        self.chunks.insert(source, function_base);
    }
}
