//! Machine-readable projection of the declarative opcode schema.
//!
//! # Contents
//! - [`OpcodeAudit`] exposes one stable JSON row per active opcode.
//! - [`opcode_inventory`] projects [`crate::opcode_schema::OPCODE_SCHEMA`].
//!
//! # Invariants
//! - Identity, wire format, conservative effects, and tier policy are copied
//!   from the declarative schema rather than reconstructed here.
//! - Authority markers distinguish exact schema facts from deliberately
//!   conservative effect classifications.
//! - The audit is diagnostic only and adds no dispatch/allocation hot-path work.
//!
//! # See also
//! - [`crate::opcode_schema`] for the authoritative Phase 2 metadata slice.

use serde::Serialize;

use crate::opcode_schema::OPCODE_SCHEMA;
pub use crate::opcode_schema::{
    ControlFlow, ExceptionSuccessorSpec, FeedbackKind, MetadataStatus, OperandFormat,
    RegisterAccess, RegisterSource, SuccessorSpec, TierSupport,
};

/// Exact reference to a register identifier in an operand position.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct RegisterReference {
    /// Operand position in the decoded instruction.
    pub operand_index: usize,
    /// How the operand encodes the register number.
    pub source: RegisterSource,
}

/// Exact counted register tail in a variadic operand shape.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct VariadicRegisterReference {
    /// First repeated operand position.
    pub start_operand_index: usize,
    /// Prefix operand containing the repeated-operand count.
    pub count_operand_index: usize,
    /// How each tail operand encodes its register number.
    pub source: RegisterSource,
}

/// Authority/precision markers for the audit row's metadata families.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct OpcodeAuthority {
    /// Opcode identity and byte assignment.
    pub identity: MetadataStatus,
    /// Current executable operand encoding.
    pub operand_format: MetadataStatus,
    /// Register read/write set precision.
    pub register_sets: MetadataStatus,
    /// Exact successor precision.
    pub successors: MetadataStatus,
    /// Exception/unwind successor precision.
    pub exception_successors: MetadataStatus,
    /// Execution-effect precision.
    pub effects: MetadataStatus,
    /// Interpreter/baseline/optimizer coverage policy.
    pub tier_policy: MetadataStatus,
}

/// One active-opcode audit row.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OpcodeAudit {
    /// Stable current wire byte.
    pub opcode_byte: u8,
    /// Canonical mnemonic.
    pub opcode: String,
    /// Per-family metadata precision.
    pub authority: OpcodeAuthority,
    /// Executable operand encoding.
    pub operand_format: OperandFormat,
    /// Register-read description retained for compatibility.
    pub registers_read: &'static str,
    /// Human-readable register-write summary.
    pub registers_written: &'static str,
    /// Exact schema register reads.
    pub register_reads_exact: Vec<RegisterReference>,
    /// Exact schema register writes.
    pub register_writes_exact: Vec<RegisterReference>,
    /// Exact counted register-read tails.
    pub register_read_variadic_tails: Vec<VariadicRegisterReference>,
    /// Exact counted register-write tails.
    pub register_write_variadic_tails: Vec<VariadicRegisterReference>,
    /// Coarse control-flow class.
    pub control_flow: ControlFlow,
    /// Normal-successor description.
    pub normal_successors: &'static str,
    /// Exact schema successors.
    pub successors_exact: Vec<SuccessorSpec>,
    /// Abrupt-successor description.
    pub exception_successor: &'static str,
    /// Exact schema exception successors.
    pub exception_successors_exact: Vec<ExceptionSuccessorSpec>,
    /// Whether execution may throw.
    pub may_throw: bool,
    /// Whether execution may allocate.
    pub may_allocate: bool,
    /// Whether execution may trigger moving GC.
    pub may_trigger_gc: bool,
    /// Whether execution may invoke JavaScript.
    pub may_reenter_javascript: bool,
    /// Current feedback family.
    pub feedback: FeedbackKind,
    /// Whether compiled execution needs a safepoint.
    pub safepoint_required: bool,
    /// Interpreter implementation owner.
    pub interpreter: &'static str,
    /// Baseline tier coverage.
    pub baseline: TierSupport,
    /// Optimizing tier coverage.
    pub optimizer: TierSupport,
    /// Declared machine-code fallback contract.
    pub fallback: &'static str,
}

fn register_references(
    schema: &crate::opcode_schema::OpcodeSchema,
    access: RegisterAccess,
) -> Vec<RegisterReference> {
    schema
        .operand_shape
        .prefix()
        .into_iter()
        .flatten()
        .enumerate()
        .filter(|(_, spec)| spec.register_access == access)
        .map(|(operand_index, spec)| RegisterReference {
            operand_index,
            source: spec
                .register_source
                .expect("register access always declares its source"),
        })
        .collect()
}

fn variadic_register_references(
    schema: &crate::opcode_schema::OpcodeSchema,
    access: RegisterAccess,
) -> Vec<VariadicRegisterReference> {
    let Some((count_operand_index, tail)) = schema.operand_shape.variadic() else {
        return Vec::new();
    };
    if tail.register_access != access {
        return Vec::new();
    }
    vec![VariadicRegisterReference {
        start_operand_index: schema
            .operand_shape
            .prefix()
            .expect("variadic shape has a prefix")
            .len(),
        count_operand_index,
        source: tail
            .register_source
            .expect("register tail always declares its source"),
    }]
}

/// Generate the checked active-opcode inventory.
#[must_use]
pub fn opcode_inventory() -> Vec<OpcodeAudit> {
    OPCODE_SCHEMA
        .iter()
        .map(|schema| {
            let exact_successors = schema.successor_shape.exact();
            let exact_exception_successors = schema.exception_successor_shape.exact();
            OpcodeAudit {
                opcode_byte: schema.byte,
                opcode: schema.op.mnemonic().to_owned(),
                authority: OpcodeAuthority {
                    identity: MetadataStatus::SchemaAuthoritative,
                    operand_format: MetadataStatus::SchemaAuthoritative,
                    register_sets: MetadataStatus::SchemaAuthoritative,
                    successors: MetadataStatus::SchemaAuthoritative,
                    exception_successors: MetadataStatus::SchemaAuthoritative,
                    effects: MetadataStatus::SchemaConservative,
                    tier_policy: MetadataStatus::SchemaAuthoritative,
                },
                operand_format: schema.operand_format,
                registers_read: "schema-authoritative: see register_reads_exact",
                registers_written: "schema-authoritative: see register_writes_exact",
                register_reads_exact: register_references(schema, RegisterAccess::Read),
                register_writes_exact: register_references(schema, RegisterAccess::Write),
                register_read_variadic_tails: variadic_register_references(
                    schema,
                    RegisterAccess::Read,
                ),
                register_write_variadic_tails: variadic_register_references(
                    schema,
                    RegisterAccess::Write,
                ),
                control_flow: schema.control_flow,
                normal_successors: "schema-authoritative: see successors_exact",
                successors_exact: exact_successors.to_vec(),
                exception_successor: "schema-authoritative: see exception_successors_exact",
                exception_successors_exact: exact_exception_successors.to_vec(),
                may_throw: schema.effects.may_throw,
                may_allocate: schema.effects.may_allocate,
                may_trigger_gc: schema.effects.may_trigger_gc,
                may_reenter_javascript: schema.effects.may_reenter_javascript,
                feedback: schema.feedback,
                safepoint_required: schema.effects.safepoint_required,
                interpreter: "crates/otter-vm/src/interp/dispatch.rs",
                baseline: schema.baseline,
                optimizer: schema.optimizer,
                fallback:
                    "exact-PC interpreter continuation when emitted; compile decline otherwise",
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::OP_BYTE_TABLE;

    fn row<'a>(inventory: &'a [OpcodeAudit], opcode: &str) -> &'a OpcodeAudit {
        inventory
            .iter()
            .find(|row| row.opcode == opcode)
            .unwrap_or_else(|| panic!("missing {opcode} audit row"))
    }

    #[test]
    fn inventory_covers_schema_and_compatibility_table_once() {
        let inventory = opcode_inventory();
        assert_eq!(inventory.len(), OPCODE_SCHEMA.len());
        assert_eq!(inventory.len(), OP_BYTE_TABLE.len());
        for (index, row) in inventory.iter().enumerate() {
            assert_eq!(row.opcode_byte as usize, index);
            assert_eq!(row.opcode_byte, OP_BYTE_TABLE[index].1);
            assert_eq!(row.authority.identity, MetadataStatus::SchemaAuthoritative);
            assert!(!row.opcode.is_empty());
            assert!(!row.registers_read.is_empty());
            assert!(!row.registers_written.is_empty());
            assert!(!row.normal_successors.is_empty());
        }
    }

    #[test]
    fn json_exposes_authority_and_experimental_tier_honestly() {
        let json = serde_json::to_value(opcode_inventory()).expect("audit JSON");
        let first = &json.as_array().expect("array")[0];
        assert_eq!(first["authority"]["identity"], "schema-authoritative");
        assert_eq!(first["authority"]["register_sets"], "schema-authoritative");
        assert_eq!(first["optimizer"], "experimental-only");
    }

    #[test]
    fn local_and_arithmetic_rows_expose_exact_register_sources() {
        let inventory = opcode_inventory();
        let load_local = row(&inventory, "LOAD_LOCAL");
        assert_eq!(
            load_local.authority.register_sets,
            MetadataStatus::SchemaAuthoritative
        );
        assert_eq!(
            load_local.register_reads_exact,
            vec![RegisterReference {
                operand_index: 1,
                source: RegisterSource::Imm32RegisterIndex,
            }]
        );
        assert_eq!(load_local.register_writes_exact[0].operand_index, 0);

        let add = row(&inventory, "ADD");
        assert_eq!(
            add.register_reads_exact
                .iter()
                .map(|reference| reference.operand_index)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(add.register_writes_exact[0].operand_index, 0);

        let evaluate_module = row(&inventory, "EVALUATE_MODULE");
        assert_eq!(
            evaluate_module.authority.register_sets,
            MetadataStatus::SchemaAuthoritative
        );
        assert_eq!(evaluate_module.register_writes_exact[0].operand_index, 0);
        assert!(evaluate_module.register_reads_exact.is_empty());
    }

    #[test]
    fn property_upvalue_and_iterator_rows_are_exact() {
        let inventory = opcode_inventory();
        let store_property = row(&inventory, "STORE_PROPERTY");
        assert_eq!(
            store_property
                .register_reads_exact
                .iter()
                .map(|reference| reference.operand_index)
                .collect::<Vec<_>>(),
            vec![0, 2]
        );
        assert_eq!(store_property.register_writes_exact[0].operand_index, 3);

        let load_upvalue = row(&inventory, "LOAD_UPVALUE");
        assert!(load_upvalue.register_reads_exact.is_empty());
        assert_eq!(load_upvalue.register_writes_exact[0].operand_index, 0);

        let iterator_next = row(&inventory, "ITERATOR_NEXT");
        assert_eq!(
            iterator_next
                .register_writes_exact
                .iter()
                .map(|reference| reference.operand_index)
                .collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert_eq!(iterator_next.register_reads_exact[0].operand_index, 2);
    }

    #[test]
    fn call_family_exposes_prefix_and_counted_register_tail() {
        let inventory = opcode_inventory();
        let call = row(&inventory, "CALL");
        assert_eq!(
            call.authority.register_sets,
            MetadataStatus::SchemaAuthoritative
        );
        assert_eq!(
            call.register_reads_exact,
            vec![RegisterReference {
                operand_index: 1,
                source: RegisterSource::RegisterOperand,
            }]
        );
        assert_eq!(call.register_writes_exact[0].operand_index, 0);
        assert_eq!(
            call.register_read_variadic_tails,
            vec![VariadicRegisterReference {
                start_operand_index: 3,
                count_operand_index: 2,
                source: RegisterSource::RegisterOperand,
            }]
        );

        let call_with_this = row(&inventory, "CALL_WITH_THIS");
        assert_eq!(
            call_with_this
                .register_reads_exact
                .iter()
                .map(|reference| reference.operand_index)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(
            call_with_this.register_read_variadic_tails[0].start_operand_index,
            4
        );

        let new_array = row(&inventory, "NEW_ARRAY");
        assert_eq!(new_array.register_writes_exact[0].operand_index, 0);
        assert_eq!(
            new_array.register_read_variadic_tails[0],
            VariadicRegisterReference {
                start_operand_index: 2,
                count_operand_index: 1,
                source: RegisterSource::RegisterOperand,
            }
        );

        let method = row(&inventory, "CALL_METHOD_VALUE");
        assert_eq!(method.register_reads_exact[0].operand_index, 1);
        assert_eq!(
            method.register_read_variadic_tails[0].start_operand_index,
            4
        );
    }

    #[test]
    fn branch_and_return_rows_expose_exact_successors() {
        let inventory = opcode_inventory();
        let branch = row(&inventory, "JUMP_IF_FALSE");
        assert_eq!(
            branch.authority.successors,
            MetadataStatus::SchemaAuthoritative
        );
        assert_eq!(branch.successors_exact.len(), 2);
        assert!(matches!(
            branch.successors_exact[0],
            SuccessorSpec::RelativeTarget {
                operand_index: 0,
                ..
            }
        ));
        assert_eq!(branch.successors_exact[1], SuccessorSpec::Fallthrough);

        let return_row = row(&inventory, "RETURN");
        assert_eq!(
            return_row.successors_exact,
            vec![SuccessorSpec::FrameReturn]
        );

        let call = row(&inventory, "CALL");
        assert_eq!(
            call.authority.successors,
            MetadataStatus::SchemaAuthoritative
        );
        assert_eq!(call.successors_exact, vec![SuccessorSpec::Fallthrough]);
    }

    #[test]
    fn exception_rows_distinguish_handlers_and_dynamic_unwind() {
        let inventory = opcode_inventory();
        let enter_try = row(&inventory, "ENTER_TRY");
        assert_eq!(
            enter_try.authority.exception_successors,
            MetadataStatus::SchemaAuthoritative
        );
        assert_eq!(enter_try.exception_successors_exact.len(), 2);
        assert!(matches!(
            enter_try.exception_successors_exact[0],
            ExceptionSuccessorSpec::OptionalRelativeTarget {
                operand_index: 0,
                absent_value: crate::NO_HANDLER_OFFSET,
                ..
            }
        ));

        let throw = row(&inventory, "THROW");
        assert_eq!(
            throw.exception_successors_exact,
            vec![ExceptionSuccessorSpec::DynamicFrameHandlerOrCaller]
        );
        assert!(throw.successors_exact.is_empty());

        let end_finally = row(&inventory, "END_FINALLY");
        assert_eq!(
            end_finally.exception_successors_exact,
            vec![ExceptionSuccessorSpec::ResumeParkedAbruptCompletion]
        );

        let jump_via_finally = row(&inventory, "JUMP_VIA_FINALLY");
        assert!(matches!(
            jump_via_finally.exception_successors_exact[0],
            ExceptionSuccessorSpec::RunFinallyHandlersToFloor {
                floor_operand_index: 1
            }
        ));
        assert!(matches!(
            jump_via_finally.successors_exact[0],
            SuccessorSpec::RelativeTarget {
                operand_index: 0,
                ..
            }
        ));
    }
}
