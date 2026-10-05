//! Independent execution state for captured code.
//!
//! # Contents
//! Copy admitted code and metadata while creating empty advisory feedback.
//!
//! # Invariants
//! Function IDs, instruction coordinates and control flow remain identical.
//! No IC shape, validity cell, observation or source-work total crosses an isolate boundary.
//! The admitted instruction stream is copied without parsing or revalidation.
//! Each fresh body, function table and source-work cell admits its own physical
//! lease on the supplied account; refusal drops the unpublished copy in full.
//!
//! # See also
//! `crate::code_space::snapshot` freezes and restores code directories.

use super::{CodeBlock, ExecutableModule, allocation};
use otter_resource::{ResourceAccount, ResourceClass, ResourceError};
use std::sync::Arc;

impl ExecutableModule {
    pub(crate) fn fresh_isolate_copy(
        &self,
        account: &ResourceAccount,
    ) -> Result<Self, ResourceError> {
        let mut table_lease =
            account.reserve_exact(ResourceClass::SourceModuleBytes, self._table_lease.amount())?;
        let mut functions = allocation::try_vec(self.functions.len(), &mut table_lease)?;
        for function in &self.functions {
            functions.push(Arc::new(function.fresh_isolate_copy(account)?));
        }
        let functions = functions.into_boxed_slice();
        table_lease.resize(
            (std::mem::size_of::<Self>() as u64)
                .saturating_add(allocation::array_bytes::<Arc<CodeBlock>>(functions.len())),
        )?;
        Ok(Self {
            functions,
            property_ic_site_end: self.property_ic_site_end,
            _table_lease: table_lease,
        })
    }
}

impl CodeBlock {
    fn fresh_isolate_copy(&self, account: &ResourceAccount) -> Result<Self, ResourceError> {
        // Fixed execution arrays have equal physical size in a fresh copy;
        // advisory feedback is recreated at the same declared slot geometry.
        let mut lease =
            account.reserve_exact(ResourceClass::SourceModuleBytes, self._lease.amount())?;
        let mapped_argument_bindings =
            allocation::try_copy(&self.mapped_argument_bindings, &mut lease)?;
        let module_url = allocation::try_string(&self.module_url, &mut lease)?.into_boxed_str();
        let scopes = allocation::try_scopes(&self.scopes, &mut lease)?;
        let code = allocation::try_copy(&self.code, &mut lease)?;
        let overflow_operand_words =
            allocation::try_copy(&self.overflow_operand_words, &mut lease)?;
        let control_flow = self.control_flow.fresh_copy(&mut lease)?;
        let feedback = crate::feedback::FeedbackVector::for_instruction_ops(
            self.code.iter().map(|instruction| self.op(instruction)),
            &mut lease,
        )?;
        let byte_pcs = allocation::try_copy(&self.byte_pcs, &mut lease)?;
        let byte_spans = allocation::try_copy(&self.byte_spans, &mut lease)?;
        let number_hints = allocation::try_copy(&self.number_hints, &mut lease)?;
        let class_hints = allocation::try_copy(&self.class_hints, &mut lease)?;
        let mut body = Self {
            _lease: lease,
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
            mapped_argument_bindings,
            is_module: self.is_module,
            module_url,
            scopes,
            contains_direct_eval: self.contains_direct_eval,
            code,
            overflow_operand_words,
            bytecode_byte_len: self.bytecode_byte_len,
            control_flow,
            feedback,
            source_work: Arc::new(crate::native_abi::SourceWork::new(account)?),
            byte_pcs,
            byte_spans,
            number_hints,
            class_hints,
        };
        body._lease
            .resize((std::mem::size_of::<Self>() as u64).saturating_add(body.retained_bytes()))?;
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_work_snapshots_retain_the_cell_and_new_isolates_start_with_zero_work() {
        let source = CodeBlock::jit_test_stub(7, 0, 1, &[], &[]);
        source.source_work().charge(37);
        let snapshot = source.jit_compile_snapshot();
        assert!(Arc::ptr_eq(
            source.source_work(),
            snapshot.code_block.source_work()
        ));
        assert_eq!(snapshot.code_block.source_work().total(), 37);
        let fresh = source
            .fresh_isolate_copy(&ResourceAccount::default())
            .unwrap();
        assert!(!Arc::ptr_eq(source.source_work(), fresh.source_work()));
        assert_eq!(fresh.source_work().total(), 0);
        fresh.source_work().charge(11);
        assert_eq!(source.source_work().total(), 37);
    }
}
