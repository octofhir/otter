//! Checked inventory of active bytecode effects and tier support.
//!
//! # Contents
//! - [`OpcodeAudit`] is the machine-readable Phase 0 row for one active opcode.
//! - [`opcode_inventory`] derives a row for every entry in
//!   [`crate::encoding::OP_BYTE_TABLE`].
//!
//! # Invariants
//! - Inventory membership and opcode ids come from the active encoder table, so
//!   adding/removing an opcode cannot silently leave the audit stale.
//! - Unknown effects are conservative: they require throw/allocation/GC/reentry
//!   handling and a safepoint rather than claiming an unsafe leaf path.
//! - The current self-describing encoding has no authoritative static operand
//!   schema. Rows say so explicitly; Phase 2 replaces that gap with generated
//!   exact read/write formats without maintaining a second execution bytecode.

use serde::Serialize;

use crate::{Op, encoding::OP_BYTE_TABLE};

/// Current operand encoding classification.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum OperandFormat {
    /// Operand kind bytes are embedded in each instruction; no opcode schema
    /// currently defines a static format.
    SelfDescribing,
}

/// Current control-flow shape.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ControlFlow {
    /// Successor is the next instruction.
    Fallthrough,
    /// One relative target.
    Jump,
    /// Relative target plus fallthrough.
    Branch,
    /// Call-like operation returns to the next instruction.
    Call,
    /// Function/frame completion.
    Return,
    /// Explicit throw/unwind.
    Throw,
    /// Suspend/resume boundary.
    Suspend,
    /// Static and abrupt-completion successors.
    ExceptionRegion,
}

/// Feedback family currently associated with an opcode.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum FeedbackKind {
    /// No current feedback cell.
    None,
    /// Arithmetic operand/result feedback.
    Arithmetic,
    /// Named-property inline cache.
    Property,
    /// Element/array/typed-array feedback.
    Element,
    /// Call target/arity feedback.
    Call,
    /// Global/dynamic environment feedback.
    Global,
}

/// Current machine-code tier coverage.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum TierSupport {
    /// The current emitter has a native or runtime-stub path for common cases.
    Partial,
    /// The current tier declines or exits to the interpreter.
    Fallback,
    /// Tier is excluded from normal builds and retained only experimentally.
    ExperimentalOnly,
}

/// One checked active-opcode audit row.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct OpcodeAudit {
    /// Stable current wire byte.
    pub opcode_byte: u8,
    /// Mnemonic.
    pub opcode: String,
    /// Operand wire format.
    pub operand_format: OperandFormat,
    /// Register read description. Exact static sets do not exist in the current
    /// encoding and are recovered by each consumer from the operand list.
    pub registers_read: &'static str,
    /// Register write description; same current-schema limitation as reads.
    pub registers_written: &'static str,
    /// Normal control-flow successors.
    pub control_flow: ControlFlow,
    /// Exception successor description.
    pub exception_successor: &'static str,
    /// May throw under conservative current semantics.
    pub may_throw: bool,
    /// May allocate under conservative current semantics.
    pub may_allocate: bool,
    /// May trigger moving GC.
    pub may_trigger_gc: bool,
    /// May invoke JavaScript/proxy/accessor/coercion behavior.
    pub may_reenter_javascript: bool,
    /// Current feedback family.
    pub feedback: FeedbackKind,
    /// Whether compiled execution must publish a safepoint before its slow path.
    pub safepoint_required: bool,
    /// Interpreter implementation owner.
    pub interpreter: &'static str,
    /// Current baseline coverage.
    pub baseline: TierSupport,
    /// Current optimizing-tier coverage.
    pub optimizer: TierSupport,
    /// Declared fallback behavior.
    pub fallback: &'static str,
}

fn control_flow(op: Op) -> ControlFlow {
    match op {
        Op::Jump | Op::JumpViaFinally => ControlFlow::Jump,
        Op::JumpIfTrue | Op::JumpIfFalse | Op::JumpIfNullish => ControlFlow::Branch,
        Op::Return | Op::ReturnValue | Op::ReturnUndefined | Op::TailCall => ControlFlow::Return,
        Op::Throw => ControlFlow::Throw,
        Op::EnterTry | Op::LeaveTry | Op::EndFinally | Op::PopParkedFinally => {
            ControlFlow::ExceptionRegion
        }
        Op::Await | Op::Yield | Op::YieldDelegate | Op::GeneratorStart => ControlFlow::Suspend,
        Op::Call
        | Op::CallWithThis
        | Op::CallMethodValue
        | Op::CallSpread
        | Op::New
        | Op::NewSpread
        | Op::SuperConstructSpread
        | Op::Eval
        | Op::MathCall
        | Op::PromiseCall => ControlFlow::Call,
        _ => ControlFlow::Fallthrough,
    }
}

fn feedback(op: Op) -> FeedbackKind {
    match op {
        Op::Add
        | Op::Sub
        | Op::Mul
        | Op::Div
        | Op::Rem
        | Op::Pow
        | Op::Increment
        | Op::LessThan
        | Op::LessEq
        | Op::GreaterThan
        | Op::GreaterEq => FeedbackKind::Arithmetic,
        Op::LoadProperty | Op::StoreProperty | Op::HasProperty | Op::DeleteProperty => {
            FeedbackKind::Property
        }
        Op::LoadElement | Op::StoreElement | Op::DeleteElement | Op::ArrayLength => {
            FeedbackKind::Element
        }
        Op::Call | Op::CallWithThis | Op::CallMethodValue | Op::TailCall => FeedbackKind::Call,
        Op::LoadGlobalOrThrow
        | Op::LoadGlobalOrUndefined
        | Op::StoreGlobalBinding
        | Op::LoadDynamic
        | Op::StoreDynamic => FeedbackKind::Global,
        _ => FeedbackKind::None,
    }
}

fn proven_leaf(op: Op) -> bool {
    matches!(
        op,
        Op::Nop
            | Op::LoadUndefined
            | Op::LoadHole
            | Op::LoadTrue
            | Op::LoadFalse
            | Op::LoadNull
            | Op::LoadLocal
            | Op::StoreLocal
            | Op::LoadThis
            | Op::LoadNewTarget
            | Op::Jump
            | Op::JumpIfTrue
            | Op::JumpIfFalse
            | Op::JumpIfNullish
            | Op::LeaveTry
            | Op::Return
            | Op::ReturnValue
            | Op::ReturnUndefined
    )
}

fn baseline_support(op: Op) -> TierSupport {
    match op {
        Op::Nop
        | Op::LoadUndefined
        | Op::LoadTrue
        | Op::LoadFalse
        | Op::LoadNull
        | Op::LoadString
        | Op::LoadNumber
        | Op::LoadInt32
        | Op::Add
        | Op::Sub
        | Op::Mul
        | Op::Div
        | Op::Rem
        | Op::Neg
        | Op::Equal
        | Op::NotEqual
        | Op::LessThan
        | Op::LessEq
        | Op::GreaterThan
        | Op::GreaterEq
        | Op::Jump
        | Op::JumpIfTrue
        | Op::JumpIfFalse
        | Op::JumpIfNullish
        | Op::LoadLocal
        | Op::StoreLocal
        | Op::LoadProperty
        | Op::StoreProperty
        | Op::LoadElement
        | Op::StoreElement
        | Op::ArrayLength
        | Op::Call
        | Op::CallWithThis
        | Op::CallMethodValue
        | Op::Return
        | Op::ReturnValue
        | Op::ReturnUndefined => TierSupport::Partial,
        _ => TierSupport::Fallback,
    }
}

/// Generate the checked active-opcode inventory.
#[must_use]
pub fn opcode_inventory() -> Vec<OpcodeAudit> {
    OP_BYTE_TABLE
        .iter()
        .map(|(op, byte)| {
            let leaf = proven_leaf(*op);
            OpcodeAudit {
                opcode_byte: *byte,
                opcode: op.mnemonic().to_owned(),
                operand_format: OperandFormat::SelfDescribing,
                registers_read: "decoded per instruction; no authoritative opcode schema",
                registers_written: "decoded per instruction; no authoritative opcode schema",
                control_flow: control_flow(*op),
                exception_successor: if leaf { "none" } else { "dynamic frame handler or caller" },
                may_throw: !leaf,
                may_allocate: !leaf,
                may_trigger_gc: !leaf,
                may_reenter_javascript: !leaf,
                feedback: feedback(*op),
                safepoint_required: !leaf,
                interpreter: "crates/otter-vm/src/interp/dispatch.rs",
                baseline: baseline_support(*op),
                optimizer: TierSupport::ExperimentalOnly,
                fallback: "exact-PC interpreter continuation when emitted; compile decline otherwise",
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventory_covers_every_active_opcode_once() {
        let inventory = opcode_inventory();
        assert_eq!(inventory.len(), OP_BYTE_TABLE.len());
        for (index, row) in inventory.iter().enumerate() {
            assert_eq!(row.opcode_byte as usize, index);
            assert!(!row.opcode.is_empty());
            assert!(!row.registers_read.is_empty());
            assert!(!row.registers_written.is_empty());
            assert!(!row.interpreter.is_empty());
            assert!(!row.fallback.is_empty());
        }
    }

    #[test]
    fn conservative_effects_always_require_safepoints() {
        for row in opcode_inventory() {
            if row.may_allocate || row.may_trigger_gc || row.may_reenter_javascript {
                assert!(row.safepoint_required, "{}", row.opcode);
            }
        }
    }
}
