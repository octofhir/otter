//! Independent execution state for captured code.
//!
//! # Contents
//! Copy admitted code and metadata while creating empty advisory feedback.
//!
//! # Invariants
//! Function IDs, instruction coordinates and control flow remain identical.
//! No IC shape, validity cell or observation crosses an isolate boundary.
//! The admitted instruction stream is copied without parsing or revalidation.
//!
//! # See also
//! `crate::code_space::snapshot` freezes and restores code directories.

use super::{CodeBlock, ExecutableModule};
use std::sync::Arc;

impl ExecutableModule {
    pub(crate) fn fresh_isolate_copy(&self) -> Self {
        Self {
            functions: self
                .functions
                .iter()
                .map(|function| Arc::new(function.fresh_isolate_copy()))
                .collect(),
            property_ic_site_end: self.property_ic_site_end,
        }
    }
}

impl CodeBlock {
    fn fresh_isolate_copy(&self) -> Self {
        Self {
            id: self.id,
            param_count: self.param_count,
            register_count: self.register_count,
            is_strict: self.is_strict,
            is_arrow: self.is_arrow,
            is_method: self.is_method,
            has_rest: self.has_rest,
            is_async: self.is_async,
            is_generator: self.is_generator,
            is_async_generator: self.is_async_generator,
            is_derived_constructor: self.is_derived_constructor,
            makes_function: self.makes_function,
            observes_this: self.observes_this,
            needs_arguments: self.needs_arguments,
            arguments_object_kind: self.arguments_object_kind,
            mapped_argument_bindings: self.mapped_argument_bindings.clone(),
            is_module: self.is_module,
            module_url: self.module_url.clone(),
            scopes: self.scopes.clone(),
            contains_direct_eval: self.contains_direct_eval,
            code: self.code.clone(),
            overflow_operand_words: self.overflow_operand_words.clone(),
            bytecode_byte_len: self.bytecode_byte_len,
            control_flow: self.control_flow.clone(),
            feedback: crate::feedback::FeedbackVector::for_instruction_ops(
                self.code.iter().map(|instruction| self.op(instruction)),
            ),
            byte_pcs: self.byte_pcs.clone(),
            byte_spans: self.byte_spans.clone(),
            number_hints: self.number_hints.clone(),
            class_hints: self.class_hints.clone(),
        }
    }
}
