//! Source-chunk leases for prepared optimizing compilations.
//!
//! # Contents
//! - [`CompilationSources`] retains the defining contexts of every prepared
//!   body and checks the actually emitted source identities before publication.
//!
//! # Invariants
//! An immutable CodeBlock alone does not retain its linked chunk. Prepared
//! bodies therefore keep their exact context owners until the worker's terminal
//! acknowledgement. These leases stay on the isolate thread. Function ids are
//! meaningful only inside their original CodeSpace, including lazy aliases;
//! publication verifies the current space and executable owner independently of
//! cancellation and speculative dependency admission.
//!
//! # See also
//! - [`crate::jit_compile_queue`] owns pending jobs and acknowledgement.
//! - [`crate::code_space`] owns live payloads and immutable eviction tombstones.

use std::{collections::BTreeMap, sync::Arc};

use crate::{CodeBlock, ExecutionContext, JitCompileSnapshot, code_space::CodeSpace};

/// Isolate-owned source leases, released only after terminal acknowledgement.
pub(crate) struct CompilationSources {
    owners: Box<[ExecutionContext]>,
}

impl CompilationSources {
    /// Retain every prepared body, including candidates the backend may reject.
    pub(crate) fn retain(
        context: &ExecutionContext,
        snapshot: &JitCompileSnapshot,
    ) -> Option<Self> {
        let mut owners = BTreeMap::new();
        let mut pending = vec![snapshot];
        while let Some(view) = pending.pop() {
            let function_id = view.code_block.id;
            let owner = ExecutionContext::for_function_in(context.space(), function_id).ok()?;
            if !std::ptr::eq(owner.exec_function(function_id)?, view.code_block.as_ref()) {
                return None;
            }
            owners.entry(owner.function_base()).or_insert(owner);
            // Equal function ids can carry different bounded prepared trees;
            // deduplicate retained owners, never the traversal of those trees.
            pending.extend(
                view.inline_callees
                    .values()
                    .map(|callee| callee.body.as_ref()),
            );
            pending.extend(
                view.inline_methods
                    .values()
                    .map(|method| method.body.as_ref()),
            );
            pending.extend(
                view.inline_poly_methods
                    .values()
                    .flatten()
                    .map(|method| method.body.as_ref()),
            );
            pending.extend(
                view.direct_methods
                    .values()
                    .flatten()
                    .filter_map(|method| method.body.as_deref()),
            );
        }
        Some(Self {
            owners: owners.into_values().collect(),
        })
    }

    /// Verify the owner and only those candidate bodies actually emitted.
    pub(crate) fn is_current(
        &self,
        space: &Arc<CodeSpace>,
        function: &CodeBlock,
        spliced_functions: &[u32],
    ) -> bool {
        let Some(owner) = self.owner(function.id) else {
            return false;
        };
        if !Arc::ptr_eq(space, owner.space())
            || !owner
                .exec_function(function.id)
                .is_some_and(|saved| std::ptr::eq(saved, function))
        {
            return false;
        }
        std::iter::once(function.id)
            .chain(spliced_functions.iter().copied())
            .all(|function_id| {
                let Some(saved) = self.owner(function_id) else {
                    return false;
                };
                let Ok(current) = ExecutionContext::for_function_in(space, function_id) else {
                    return false;
                };
                Arc::ptr_eq(saved.executable_module(), current.executable_module())
            })
    }

    fn owner(&self, function_id: u32) -> Option<&ExecutionContext> {
        self.owners
            .iter()
            .find(|owner| owner.resolves_locally(function_id))
    }
}

#[cfg(test)]
#[path = "jit_compile_lifetime_tests.rs"]
mod tests;
