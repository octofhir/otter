//! Address-free AArch64 relocation artifacts and semantic code normalization.
//!
//! # Contents
//! - [`RelocationCapture`] records typed address materializations while the
//!   existing emission pass is active.
//! - [`RelocationTarget`] describes the runtime meaning of an address without
//!   retaining or serializing the process-local address itself.
//! - [`RelocationCapture::render`] validates the finalized instruction bytes,
//!   renders `relocations.json`, and builds portable `code-normalized.bin`.
//!
//! # Invariants
//! - Relocation ranges use exact `code.bin` byte offsets and contain one
//!   contiguous AArch64 `MOVZ` followed by zero to three `MOVK` instructions.
//! - Captured targets contain semantic identities only. Raw target addresses
//!   never enter this module's state or either rendered artifact.
//! - Exact relocation JSON may name an isolate-local target code generation.
//!   Portable normalized code excludes that generation id while retaining the
//!   target function, tier, and complete stack-layout contract.
//! - The normalized stream collapses every variable-length materialization to
//!   one logical item. Direct branch destinations are encoded as logical-item
//!   ordinals, so address-dependent MOV-wide lengths cannot perturb them.
//! - Unknown PC-relative data materializations are rejected instead of being
//!   copied into an artifact that appears portable.
//!
//! # See also
//! - [`super`] for the owned JIT artifact bundle builder.

use std::fmt;

use otter_vm::native_abi::{RuntimeStubDescriptor, RuntimeStubSignature, runtime_stub_name};
use serde::Serialize;

use super::{
    DirectCallArtifact, DirectCallKindArtifact, DirectCallThisModeArtifact, DirectCallTierArtifact,
};

const NORMALIZED_MAGIC: &[u8; 8] = b"OTJNCODE";
const NORMALIZED_ARCH_AARCH64: u16 = 1;

const ITEM_RAW_INSTRUCTION: u8 = 0;
const ITEM_RELOCATION: u8 = 1;
const ITEM_DIRECT_BRANCH: u8 = 2;

const TARGET_RUNTIME_STUB: u8 = 1;
const TARGET_GC_CAGE_BASE: u8 = 2;
const TARGET_PROPERTY_IC_CELL: u8 = 3;
const TARGET_TEMPLATE_OPERAND_SLICE: u8 = 4;
const TARGET_OPTIMIZED_MATH_ARGUMENTS: u8 = 5;
const TARGET_GUARDED_HEAP_REFERENCE: u8 = 6;
const TARGET_GUARDED_BUILTIN_FUNCTION: u8 = 7;
const TARGET_DIRECT_CALL_ENTRY_CELL: u8 = 8;
const TARGET_STATIC_NATIVE_BUILTIN_FUNCTION: u8 = 9;
const TARGET_GLOBAL_LEXICAL_CELL: u8 = 10;

/// Whether a property inline-cache cell serves a load or a store site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum PropertyIcAccess {
    Load,
    Store,
}

/// Template-plan arena owning an address-stable operand slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum TemplateOperandArena {
    Registers,
    Indices,
}

/// Semantic role of one template-plan operand slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum TemplateOperandRole {
    ClosureParents,
    NewArrayElements,
    MathArguments,
    ConstructArguments,
}

/// Address-stable heap component used by a collection fast path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum GuardedHeapComponent {
    Prototype,
    PrototypeShape,
}

/// Which guarded builtin family a relocation belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum GuardedBuiltinKind {
    /// Non-allocating collection read.
    Leaf,
    /// Allocating collection write.
    Alloc,
    /// Primitive prototype builtin reached through a leaf entry.
    Primitive,
    /// Dense-array `push` / `pop` builtin reached through a typed entry.
    Array,
}

/// Semantic identity for one address materialized in native code.
///
/// None of these variants accepts a process-local pointer. Text fields name
/// compiler/runtime concepts and are length-framed in the normalized binary,
/// making their encoding deterministic and unambiguous.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub(crate) enum RelocationTarget {
    RuntimeStub {
        id: u32,
        name: &'static str,
        signature: &'static str,
    },
    GcCageBase,
    /// Permanent global-declarative cell read by one `LoadGlobalOrThrow`.
    GlobalLexicalCell {
        byte_pc: u32,
    },
    PropertyIcCell {
        access: PropertyIcAccess,
        ordinal: u32,
    },
    TemplateOperandSlice {
        arena: TemplateOperandArena,
        role: TemplateOperandRole,
        start: u32,
        len: u32,
    },
    OptimizedMathArguments {
        inline_frame: u32,
        logical_pc: u32,
        len: u32,
    },
    GuardedHeapReference {
        component: GuardedHeapComponent,
        feedback_kind: GuardedBuiltinKind,
        byte_pc: u32,
        runtime_stub_id: u32,
    },
    GuardedBuiltinFunction {
        feedback_kind: GuardedBuiltinKind,
        byte_pc: u32,
        runtime_stub_id: u32,
    },
    /// Exact bootstrap function identity guarded by an ordinary-call native
    /// leaf. The process address is deliberately absent.
    StaticNativeBuiltinFunction {
        target: otter_vm::JitStaticNativeCallKind,
        byte_pc: u32,
    },
    /// Stable registry cell guarded by one generated direct-call site.
    ///
    /// The process address is intentionally absent. Exact JSON retains the
    /// target generation id; normalized code omits that id and retains the
    /// portable target/layout contract.
    DirectCallEntryCell {
        byte_pc: u32,
        direct_call: DirectCallArtifact,
    },
}

impl RelocationTarget {
    /// Builds a stable symbolic identity from the authoritative ABI descriptor.
    pub(crate) fn runtime_stub(descriptor: RuntimeStubDescriptor) -> Self {
        Self::RuntimeStub {
            id: descriptor.id,
            name: runtime_stub_name(descriptor.id),
            signature: runtime_stub_signature_name(descriptor.signature),
        }
    }
}

fn runtime_stub_signature_name(signature: RuntimeStubSignature) -> &'static str {
    match signature {
        RuntimeStubSignature::LeafValue2 => "leafValue2",
        RuntimeStubSignature::AllocValue3 => "allocValue3",
        RuntimeStubSignature::Poll1 => "poll1",
        RuntimeStubSignature::Variadic => "variadic",
        RuntimeStubSignature::NullaryValue => "nullaryValue",
        RuntimeStubSignature::MutatingLeafValue2 => "mutatingLeafValue2",
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RelocationRecord {
    start: usize,
    end: usize,
    register: u8,
    target: RelocationTarget,
}

/// Optional emission-side typed relocation storage.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RelocationCapture {
    enabled: bool,
    records: Vec<RelocationRecord>,
}

impl RelocationCapture {
    /// Creates capture storage without allocating a record buffer.
    pub(crate) const fn new(enabled: bool) -> Self {
        Self {
            enabled,
            records: Vec::new(),
        }
    }

    /// Records one emitted MOV-wide address materialization.
    ///
    /// Validation is intentionally deferred until finalized code is available:
    /// `render` verifies both the byte range and every encoded instruction.
    pub(crate) fn record_mov_wide(
        &mut self,
        start: usize,
        end: usize,
        register: u8,
        target: RelocationTarget,
    ) {
        if !self.enabled {
            return;
        }
        self.records.push(RelocationRecord {
            start,
            end,
            register,
            target,
        });
    }

    /// Renders address-free relocation metadata and portable semantic code.
    pub(crate) fn render(&self, code: &[u8]) -> Result<RenderedRelocations, RelocationError> {
        if !code.len().is_multiple_of(4) {
            return Err(RelocationError::CodeLengthNotInstructionAligned {
                code_len: code.len(),
            });
        }

        let validated = ValidatedRelocations {
            records: validate_relocations(&self.records, code)?,
        };
        let logical_items = build_logical_items(&validated.records, code);
        let normalized_code = render_normalized(&validated.records, &logical_items, code)?;
        let json = render_json(&validated.records);
        Ok(RenderedRelocations {
            json,
            normalized_code,
            validated,
        })
    }
}

/// Rendered files added to an owned JIT artifact bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RenderedRelocations {
    pub(crate) json: String,
    pub(crate) normalized_code: Vec<u8>,
    pub(super) validated: ValidatedRelocations,
}

/// A relocation range or PC-relative instruction violated portability rules.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RelocationError {
    CodeLengthNotInstructionAligned {
        code_len: usize,
    },
    EmptyRange {
        start: usize,
    },
    RangeNotInstructionAligned {
        start: usize,
        end: usize,
    },
    RangeOutOfBounds {
        start: usize,
        end: usize,
        code_len: usize,
    },
    MovWideInstructionCount {
        start: usize,
        count: usize,
    },
    InvalidRegister {
        start: usize,
        register: u8,
    },
    OverlappingRanges {
        previous_start: usize,
        previous_end: usize,
        start: usize,
        end: usize,
    },
    ExpectedMovz {
        offset: usize,
    },
    ExpectedMovk {
        offset: usize,
    },
    RegisterMismatch {
        offset: usize,
        expected: u8,
        actual: u8,
    },
    WidthMismatch {
        offset: usize,
        expected_bits: u8,
        actual_bits: u8,
    },
    FirstWideShiftNotZero {
        offset: usize,
        shift_bits: u8,
    },
    InvalidWideShift {
        offset: usize,
        width_bits: u8,
        shift_bits: u8,
    },
    NonIncreasingWideShift {
        offset: usize,
        previous_shift_bits: u8,
        shift_bits: u8,
    },
    UnsupportedPcRelative {
        offset: usize,
        instruction: &'static str,
    },
    BranchTargetOutOfBounds {
        offset: usize,
        target: i64,
        code_len: usize,
    },
    BranchTargetNotInstructionAligned {
        offset: usize,
        target: usize,
    },
    BranchIntoRelocation {
        offset: usize,
        target: usize,
        relocation_start: usize,
        relocation_end: usize,
    },
    BranchTargetNotLogicalBoundary {
        offset: usize,
        target: usize,
    },
    LogicalItemCountOverflow {
        count: usize,
    },
    TargetTextTooLong {
        field: &'static str,
        len: usize,
    },
}

impl fmt::Display for RelocationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CodeLengthNotInstructionAligned { code_len } => {
                write!(
                    formatter,
                    "AArch64 code length {code_len} is not a multiple of 4"
                )
            }
            Self::EmptyRange { start } => {
                write!(
                    formatter,
                    "relocation at byte offset {start} has an empty range"
                )
            }
            Self::RangeNotInstructionAligned { start, end } => write!(
                formatter,
                "relocation range {start}..{end} is not AArch64-instruction aligned"
            ),
            Self::RangeOutOfBounds {
                start,
                end,
                code_len,
            } => write!(
                formatter,
                "relocation range {start}..{end} exceeds code length {code_len}"
            ),
            Self::MovWideInstructionCount { start, count } => write!(
                formatter,
                "relocation at byte offset {start} contains {count} MOV-wide instructions; expected 1..=4"
            ),
            Self::InvalidRegister { start, register } => write!(
                formatter,
                "relocation at byte offset {start} names invalid AArch64 register {register}"
            ),
            Self::OverlappingRanges {
                previous_start,
                previous_end,
                start,
                end,
            } => write!(
                formatter,
                "relocation ranges {previous_start}..{previous_end} and {start}..{end} overlap"
            ),
            Self::ExpectedMovz { offset } => {
                write!(formatter, "expected MOVZ at byte offset {offset}")
            }
            Self::ExpectedMovk { offset } => {
                write!(formatter, "expected MOVK at byte offset {offset}")
            }
            Self::RegisterMismatch {
                offset,
                expected,
                actual,
            } => write!(
                formatter,
                "MOV-wide register mismatch at byte offset {offset}: expected x/w{expected}, found x/w{actual}"
            ),
            Self::WidthMismatch {
                offset,
                expected_bits,
                actual_bits,
            } => write!(
                formatter,
                "MOV-wide width mismatch at byte offset {offset}: expected {expected_bits}, found {actual_bits}"
            ),
            Self::FirstWideShiftNotZero { offset, shift_bits } => write!(
                formatter,
                "MOVZ at byte offset {offset} starts at shift {shift_bits}; emitted address sequences must start at zero"
            ),
            Self::InvalidWideShift {
                offset,
                width_bits,
                shift_bits,
            } => write!(
                formatter,
                "MOV-wide shift {shift_bits} at byte offset {offset} is invalid for a {width_bits}-bit register"
            ),
            Self::NonIncreasingWideShift {
                offset,
                previous_shift_bits,
                shift_bits,
            } => write!(
                formatter,
                "MOVK shift {shift_bits} at byte offset {offset} does not follow shift {previous_shift_bits}"
            ),
            Self::UnsupportedPcRelative {
                offset,
                instruction,
            } => write!(
                formatter,
                "unsupported PC-relative {instruction} at byte offset {offset}"
            ),
            Self::BranchTargetOutOfBounds {
                offset,
                target,
                code_len,
            } => write!(
                formatter,
                "branch at byte offset {offset} targets {target}, outside 0..{code_len}"
            ),
            Self::BranchTargetNotInstructionAligned { offset, target } => write!(
                formatter,
                "branch at byte offset {offset} targets unaligned byte offset {target}"
            ),
            Self::BranchIntoRelocation {
                offset,
                target,
                relocation_start,
                relocation_end,
            } => write!(
                formatter,
                "branch at byte offset {offset} targets {target}, inside relocation {relocation_start}..{relocation_end}"
            ),
            Self::BranchTargetNotLogicalBoundary { offset, target } => write!(
                formatter,
                "branch at byte offset {offset} targets byte offset {target}, which is not a logical-item boundary"
            ),
            Self::LogicalItemCountOverflow { count } => {
                write!(
                    formatter,
                    "normalized code has too many logical items: {count}"
                )
            }
            Self::TargetTextTooLong { field, len } => write!(
                formatter,
                "relocation target field {field} is too large to encode ({len} bytes)"
            ),
        }
    }
}

impl std::error::Error for RelocationError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
enum MovWideOperation {
    Movz,
    Movk,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
struct MovWideChunk {
    instruction_offset: u64,
    operation: MovWideOperation,
    shift_bits: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ValidatedRelocation {
    pub(super) start_offset: u64,
    pub(super) end_offset: u64,
    pub(super) register: u8,
    pub(super) width_bits: u8,
    chunks: Vec<MovWideChunk>,
    pub(super) target: RelocationTarget,
}

impl ValidatedRelocation {
    fn start(&self) -> usize {
        self.start_offset as usize
    }

    fn end(&self) -> usize {
        self.end_offset as usize
    }
}

/// Validated address sites shared by every renderer in one artifact build.
///
/// The emission-side records are sorted and decoded exactly once. Both the
/// portable relocation files and annotated assembly borrow this immutable DTO.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ValidatedRelocations {
    pub(super) records: Vec<ValidatedRelocation>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogicalItem {
    RawInstruction { offset: usize },
    Relocation { index: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MovWideKind {
    Movz,
    Movk,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DecodedMovWide {
    kind: MovWideKind,
    register: u8,
    width_bits: u8,
    shift_bits: u8,
}

fn validate_relocations(
    records: &[RelocationRecord],
    code: &[u8],
) -> Result<Vec<ValidatedRelocation>, RelocationError> {
    let mut records = records.to_vec();
    records.sort_by(|left, right| {
        (left.start, left.end, left.register).cmp(&(right.start, right.end, right.register))
    });

    let mut previous: Option<&RelocationRecord> = None;
    for record in &records {
        validate_record_bounds(record, code.len())?;
        if let Some(previous) = previous
            && record.start < previous.end
        {
            return Err(RelocationError::OverlappingRanges {
                previous_start: previous.start,
                previous_end: previous.end,
                start: record.start,
                end: record.end,
            });
        }
        previous = Some(record);
    }

    records
        .into_iter()
        .map(|record| validate_mov_wide(record, code))
        .collect()
}

fn validate_record_bounds(
    record: &RelocationRecord,
    code_len: usize,
) -> Result<(), RelocationError> {
    if record.start == record.end {
        return Err(RelocationError::EmptyRange {
            start: record.start,
        });
    }
    if !record.start.is_multiple_of(4) || !record.end.is_multiple_of(4) {
        return Err(RelocationError::RangeNotInstructionAligned {
            start: record.start,
            end: record.end,
        });
    }
    if record.start > record.end || record.end > code_len {
        return Err(RelocationError::RangeOutOfBounds {
            start: record.start,
            end: record.end,
            code_len,
        });
    }
    let instruction_count = (record.end - record.start) / 4;
    if !(1..=4).contains(&instruction_count) {
        return Err(RelocationError::MovWideInstructionCount {
            start: record.start,
            count: instruction_count,
        });
    }
    if record.register > 31 {
        return Err(RelocationError::InvalidRegister {
            start: record.start,
            register: record.register,
        });
    }
    Ok(())
}

fn validate_mov_wide(
    record: RelocationRecord,
    code: &[u8],
) -> Result<ValidatedRelocation, RelocationError> {
    let instruction_count = (record.end - record.start) / 4;
    let mut chunks = Vec::with_capacity(instruction_count);
    let mut width_bits = 0;
    let mut previous_shift = 0;

    for index in 0..instruction_count {
        let offset = record.start + index * 4;
        let instruction = read_instruction(code, offset);
        let Some(decoded) = decode_mov_wide(instruction) else {
            return Err(if index == 0 {
                RelocationError::ExpectedMovz { offset }
            } else {
                RelocationError::ExpectedMovk { offset }
            });
        };
        let expected_kind = if index == 0 {
            MovWideKind::Movz
        } else {
            MovWideKind::Movk
        };
        if decoded.kind != expected_kind {
            return Err(if index == 0 {
                RelocationError::ExpectedMovz { offset }
            } else {
                RelocationError::ExpectedMovk { offset }
            });
        }
        if decoded.register != record.register {
            return Err(RelocationError::RegisterMismatch {
                offset,
                expected: record.register,
                actual: decoded.register,
            });
        }
        if decoded.shift_bits >= decoded.width_bits {
            return Err(RelocationError::InvalidWideShift {
                offset,
                width_bits: decoded.width_bits,
                shift_bits: decoded.shift_bits,
            });
        }
        if index == 0 {
            width_bits = decoded.width_bits;
            if decoded.shift_bits != 0 {
                return Err(RelocationError::FirstWideShiftNotZero {
                    offset,
                    shift_bits: decoded.shift_bits,
                });
            }
        } else {
            if decoded.width_bits != width_bits {
                return Err(RelocationError::WidthMismatch {
                    offset,
                    expected_bits: width_bits,
                    actual_bits: decoded.width_bits,
                });
            }
            if decoded.shift_bits <= previous_shift {
                return Err(RelocationError::NonIncreasingWideShift {
                    offset,
                    previous_shift_bits: previous_shift,
                    shift_bits: decoded.shift_bits,
                });
            }
        }
        previous_shift = decoded.shift_bits;
        chunks.push(MovWideChunk {
            instruction_offset: offset as u64,
            operation: match decoded.kind {
                MovWideKind::Movz => MovWideOperation::Movz,
                MovWideKind::Movk => MovWideOperation::Movk,
            },
            shift_bits: decoded.shift_bits,
        });
    }

    Ok(ValidatedRelocation {
        start_offset: record.start as u64,
        end_offset: record.end as u64,
        register: record.register,
        width_bits,
        chunks,
        target: record.target,
    })
}

fn decode_mov_wide(instruction: u32) -> Option<DecodedMovWide> {
    let kind = match instruction & 0x7f80_0000 {
        0x5280_0000 => MovWideKind::Movz,
        0x7280_0000 => MovWideKind::Movk,
        _ => return None,
    };
    let width_bits = if instruction >> 31 == 0 { 32 } else { 64 };
    Some(DecodedMovWide {
        kind,
        register: (instruction & 0x1f) as u8,
        width_bits,
        shift_bits: (((instruction >> 21) & 0x3) * 16) as u8,
    })
}

fn build_logical_items(relocations: &[ValidatedRelocation], code: &[u8]) -> Vec<LogicalItem> {
    let mut items = Vec::new();
    let mut offset = 0;
    let mut relocation_index = 0;
    while offset < code.len() {
        if relocation_index < relocations.len() && relocations[relocation_index].start() == offset {
            items.push(LogicalItem::Relocation {
                index: relocation_index,
            });
            offset = relocations[relocation_index].end();
            relocation_index += 1;
        } else {
            items.push(LogicalItem::RawInstruction { offset });
            offset += 4;
        }
    }
    items
}

fn render_json(relocations: &[ValidatedRelocation]) -> String {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Document<'a> {
        offset_basis: &'static str,
        address_encoding: &'static str,
        relocations: &'a [ValidatedRelocation],
    }

    let document = Document {
        offset_basis: "code.bin",
        address_encoding: "symbolicOnly",
        relocations,
    };
    let mut rendered =
        serde_json::to_string_pretty(&document).expect("relocation DTO always serializes");
    rendered.push('\n');
    rendered
}

fn render_normalized(
    relocations: &[ValidatedRelocation],
    items: &[LogicalItem],
    code: &[u8],
) -> Result<Vec<u8>, RelocationError> {
    let item_count = u32::try_from(items.len())
        .map_err(|_| RelocationError::LogicalItemCountOverflow { count: items.len() })?;
    let mut offsets = Vec::with_capacity(items.len());
    for item in items {
        offsets.push(match item {
            LogicalItem::RawInstruction { offset } => *offset,
            LogicalItem::Relocation { index } => relocations[*index].start(),
        });
    }
    let mut output = Vec::with_capacity(14 + items.len() * 8);
    output.extend_from_slice(NORMALIZED_MAGIC);
    put_u16(&mut output, NORMALIZED_ARCH_AARCH64);
    put_u32(&mut output, item_count);

    for item in items {
        match item {
            LogicalItem::Relocation { index } => {
                let relocation = &relocations[*index];
                output.push(ITEM_RELOCATION);
                output.push(relocation.register);
                output.push(relocation.width_bits);
                encode_target(&relocation.target, &mut output)?;
            }
            LogicalItem::RawInstruction { offset } => {
                let instruction = read_instruction(code, *offset);
                if let Some(branch) = decode_direct_branch(instruction, *offset) {
                    let target_ordinal =
                        branch_target_ordinal(branch.target, *offset, relocations, &offsets, code)?;
                    output.push(ITEM_DIRECT_BRANCH);
                    branch.encode_without_target(&mut output);
                    put_u32(&mut output, target_ordinal);
                } else if let Some(instruction) = unsupported_pc_relative(instruction) {
                    return Err(RelocationError::UnsupportedPcRelative {
                        offset: *offset,
                        instruction,
                    });
                } else {
                    output.push(ITEM_RAW_INSTRUCTION);
                    output.extend_from_slice(&code[*offset..*offset + 4]);
                }
            }
        }
    }
    Ok(output)
}

fn branch_target_ordinal(
    target: i64,
    source_offset: usize,
    relocations: &[ValidatedRelocation],
    logical_offsets: &[usize],
    code: &[u8],
) -> Result<u32, RelocationError> {
    if target < 0 || target >= code.len() as i64 {
        return Err(RelocationError::BranchTargetOutOfBounds {
            offset: source_offset,
            target,
            code_len: code.len(),
        });
    }
    let target = target as usize;
    if !target.is_multiple_of(4) {
        return Err(RelocationError::BranchTargetNotInstructionAligned {
            offset: source_offset,
            target,
        });
    }
    if let Some(relocation) = relocations
        .iter()
        .find(|relocation| target > relocation.start() && target < relocation.end())
    {
        return Err(RelocationError::BranchIntoRelocation {
            offset: source_offset,
            target,
            relocation_start: relocation.start(),
            relocation_end: relocation.end(),
        });
    }
    let ordinal = logical_offsets.binary_search(&target).map_err(|_| {
        RelocationError::BranchTargetNotLogicalBoundary {
            offset: source_offset,
            target,
        }
    })?;
    u32::try_from(ordinal).map_err(|_| RelocationError::LogicalItemCountOverflow {
        count: logical_offsets.len() - 1,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DirectBranchKind {
    B,
    Bl,
    BCond { condition: u8 },
    Cbz { is_64_bit: bool, register: u8 },
    Cbnz { is_64_bit: bool, register: u8 },
    Tbz { bit: u8, register: u8 },
    Tbnz { bit: u8, register: u8 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct DirectBranch {
    pub(super) kind: DirectBranchKind,
    pub(super) target: i64,
}

impl DirectBranch {
    fn encode_without_target(self, output: &mut Vec<u8>) {
        match self.kind {
            DirectBranchKind::B => output.push(0),
            DirectBranchKind::Bl => output.push(1),
            DirectBranchKind::BCond { condition } => {
                output.push(2);
                output.push(condition);
            }
            DirectBranchKind::Cbz {
                is_64_bit,
                register,
            } => {
                output.push(3);
                output.push(u8::from(is_64_bit));
                output.push(register);
            }
            DirectBranchKind::Cbnz {
                is_64_bit,
                register,
            } => {
                output.push(4);
                output.push(u8::from(is_64_bit));
                output.push(register);
            }
            DirectBranchKind::Tbz { bit, register } => {
                output.push(5);
                output.push(bit);
                output.push(register);
            }
            DirectBranchKind::Tbnz { bit, register } => {
                output.push(6);
                output.push(bit);
                output.push(register);
            }
        }
    }
}

pub(super) fn decode_direct_branch(instruction: u32, offset: usize) -> Option<DirectBranch> {
    if instruction & 0x7c00_0000 == 0x1400_0000 {
        let displacement = sign_extend(instruction & 0x03ff_ffff, 26) << 2;
        return Some(DirectBranch {
            kind: if instruction >> 31 == 0 {
                DirectBranchKind::B
            } else {
                DirectBranchKind::Bl
            },
            target: offset as i64 + displacement,
        });
    }
    if instruction & 0xff00_0010 == 0x5400_0000 {
        let displacement = sign_extend((instruction >> 5) & 0x7ffff, 19) << 2;
        return Some(DirectBranch {
            kind: DirectBranchKind::BCond {
                condition: (instruction & 0xf) as u8,
            },
            target: offset as i64 + displacement,
        });
    }
    if instruction & 0x7e00_0000 == 0x3400_0000 {
        let displacement = sign_extend((instruction >> 5) & 0x7ffff, 19) << 2;
        let is_nonzero = instruction & (1 << 24) != 0;
        let is_64_bit = instruction >> 31 != 0;
        let register = (instruction & 0x1f) as u8;
        return Some(DirectBranch {
            kind: if is_nonzero {
                DirectBranchKind::Cbnz {
                    is_64_bit,
                    register,
                }
            } else {
                DirectBranchKind::Cbz {
                    is_64_bit,
                    register,
                }
            },
            target: offset as i64 + displacement,
        });
    }
    if instruction & 0x7e00_0000 == 0x3600_0000 {
        let displacement = sign_extend((instruction >> 5) & 0x3fff, 14) << 2;
        let is_nonzero = instruction & (1 << 24) != 0;
        let bit = ((((instruction >> 31) & 1) << 5) | ((instruction >> 19) & 0x1f)) as u8;
        let register = (instruction & 0x1f) as u8;
        return Some(DirectBranch {
            kind: if is_nonzero {
                DirectBranchKind::Tbnz { bit, register }
            } else {
                DirectBranchKind::Tbz { bit, register }
            },
            target: offset as i64 + displacement,
        });
    }
    None
}

fn unsupported_pc_relative(instruction: u32) -> Option<&'static str> {
    match instruction & 0x9f00_0000 {
        0x1000_0000 => return Some("ADR"),
        0x9000_0000 => return Some("ADRP"),
        _ => {}
    }
    if instruction & 0x3b00_0000 == 0x1800_0000 {
        return Some("literal load/prefetch");
    }
    if instruction & 0xff00_0000 == 0x5400_0000 && instruction & 0x10 != 0 {
        return Some("BC.cond");
    }
    None
}

fn encode_target(target: &RelocationTarget, output: &mut Vec<u8>) -> Result<(), RelocationError> {
    match target {
        RelocationTarget::RuntimeStub {
            id,
            name,
            signature,
        } => {
            output.push(TARGET_RUNTIME_STUB);
            put_u32(output, *id);
            put_text(output, "runtimeStub.name", name)?;
            put_text(output, "runtimeStub.signature", signature)?;
        }
        RelocationTarget::GcCageBase => output.push(TARGET_GC_CAGE_BASE),
        RelocationTarget::GlobalLexicalCell { byte_pc } => {
            output.push(TARGET_GLOBAL_LEXICAL_CELL);
            put_u32(output, *byte_pc);
        }
        RelocationTarget::PropertyIcCell { access, ordinal } => {
            output.push(TARGET_PROPERTY_IC_CELL);
            output.push(match access {
                PropertyIcAccess::Load => 0,
                PropertyIcAccess::Store => 1,
            });
            put_u32(output, *ordinal);
        }
        RelocationTarget::TemplateOperandSlice {
            arena,
            role,
            start,
            len,
        } => {
            output.push(TARGET_TEMPLATE_OPERAND_SLICE);
            output.push(match arena {
                TemplateOperandArena::Registers => 0,
                TemplateOperandArena::Indices => 1,
            });
            output.push(match role {
                TemplateOperandRole::ClosureParents => 0,
                TemplateOperandRole::NewArrayElements => 1,
                TemplateOperandRole::MathArguments => 2,
                TemplateOperandRole::ConstructArguments => 3,
            });
            put_u32(output, *start);
            put_u32(output, *len);
        }
        RelocationTarget::OptimizedMathArguments {
            inline_frame,
            logical_pc,
            len,
        } => {
            output.push(TARGET_OPTIMIZED_MATH_ARGUMENTS);
            put_u32(output, *inline_frame);
            put_u32(output, *logical_pc);
            put_u32(output, *len);
        }
        RelocationTarget::GuardedHeapReference {
            component,
            feedback_kind,
            byte_pc,
            runtime_stub_id,
        } => {
            output.push(TARGET_GUARDED_HEAP_REFERENCE);
            output.push(match component {
                GuardedHeapComponent::Prototype => 0,
                GuardedHeapComponent::PrototypeShape => 1,
            });
            output.push(match feedback_kind {
                GuardedBuiltinKind::Leaf => 0,
                GuardedBuiltinKind::Alloc => 1,
                GuardedBuiltinKind::Primitive => 2,
                GuardedBuiltinKind::Array => 3,
            });
            put_u32(output, *byte_pc);
            put_u32(output, *runtime_stub_id);
        }
        RelocationTarget::GuardedBuiltinFunction {
            feedback_kind,
            byte_pc,
            runtime_stub_id,
        } => {
            output.push(TARGET_GUARDED_BUILTIN_FUNCTION);
            output.push(match feedback_kind {
                GuardedBuiltinKind::Leaf => 0,
                GuardedBuiltinKind::Alloc => 1,
                GuardedBuiltinKind::Primitive => 2,
                GuardedBuiltinKind::Array => 3,
            });
            put_u32(output, *byte_pc);
            put_u32(output, *runtime_stub_id);
        }
        RelocationTarget::StaticNativeBuiltinFunction { target, byte_pc } => {
            output.push(TARGET_STATIC_NATIVE_BUILTIN_FUNCTION);
            put_u32(output, *target as u32);
            put_u32(output, *byte_pc);
        }
        RelocationTarget::DirectCallEntryCell {
            byte_pc,
            direct_call,
        } => {
            output.push(TARGET_DIRECT_CALL_ENTRY_CELL);
            put_u32(output, direct_call.target_function_id);
            put_u32(output, *byte_pc);
            output.push(match direct_call.call_kind {
                DirectCallKindArtifact::Plain => 0,
                DirectCallKindArtifact::Method => 1,
            });
            output.push(match direct_call.target_tier {
                DirectCallTierArtifact::Template => 0,
                DirectCallTierArtifact::Optimizing => 1,
            });
            output.push(match direct_call.this_mode {
                DirectCallThisModeArtifact::StrictOrLexical => 0,
                DirectCallThisModeArtifact::SloppyGlobal => 1,
                DirectCallThisModeArtifact::MethodReceiver => 2,
            });
            put_u32(output, direct_call.callee_native_frame_bytes);
            put_u32(output, direct_call.linkage_bytes);
            put_u32(output, direct_call.reserved_stack_bytes);
            put_u16(output, direct_call.callee_register_count);
        }
    }
    Ok(())
}

fn put_text(output: &mut Vec<u8>, field: &'static str, text: &str) -> Result<(), RelocationError> {
    let len = u32::try_from(text.len()).map_err(|_| RelocationError::TargetTextTooLong {
        field,
        len: text.len(),
    })?;
    put_u32(output, len);
    output.extend_from_slice(text.as_bytes());
    Ok(())
}

fn put_u16(output: &mut Vec<u8>, value: u16) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn put_u32(output: &mut Vec<u8>, value: u32) {
    output.extend_from_slice(&value.to_le_bytes());
}

fn read_instruction(code: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        code[offset..offset + 4]
            .try_into()
            .expect("validated AArch64 instruction range"),
    )
}

fn sign_extend(value: u32, bits: u32) -> i64 {
    let shift = 64 - bits;
    (i64::from(value) << shift) >> shift
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::Value;

    use super::*;

    const NOP: u32 = 0xd503_201f;
    const RET: u32 = 0xd65f_03c0;

    fn runtime_stub() -> RelocationTarget {
        RelocationTarget::runtime_stub(otter_vm::native_abi::STUB_JIT_BACKEDGE_POLL)
    }

    fn direct_call_target(
        target_code_object_id: u64,
        target_tier: DirectCallTierArtifact,
        this_mode: DirectCallThisModeArtifact,
    ) -> RelocationTarget {
        RelocationTarget::DirectCallEntryCell {
            byte_pc: 13,
            direct_call: DirectCallArtifact {
                call_kind: DirectCallKindArtifact::Plain,
                target_function_id: 12,
                target_code_object_id,
                target_tier,
                this_mode,
                callee_native_frame_bytes: 160,
                linkage_bytes: 112,
                reserved_stack_bytes: 272,
                callee_register_count: 6,
            },
        }
    }

    fn instructions(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|word| word.to_le_bytes()).collect()
    }

    fn movz(register: u8, immediate: u16, shift_bits: u8, is_64_bit: bool) -> u32 {
        let base = if is_64_bit { 0xd280_0000 } else { 0x5280_0000 };
        base | (u32::from(shift_bits / 16) << 21)
            | (u32::from(immediate) << 5)
            | u32::from(register)
    }

    fn movk(register: u8, immediate: u16, shift_bits: u8, is_64_bit: bool) -> u32 {
        let base = if is_64_bit { 0xf280_0000 } else { 0x7280_0000 };
        base | (u32::from(shift_bits / 16) << 21)
            | (u32::from(immediate) << 5)
            | u32::from(register)
    }

    fn b(displacement: i32, link: bool) -> u32 {
        let immediate = ((displacement >> 2) as u32) & 0x03ff_ffff;
        (if link { 0x9400_0000 } else { 0x1400_0000 }) | immediate
    }

    fn b_cond(displacement: i32, condition: u8) -> u32 {
        0x5400_0000 | ((((displacement >> 2) as u32) & 0x7ffff) << 5) | u32::from(condition)
    }

    fn cb(displacement: i32, nonzero: bool, is_64_bit: bool, register: u8) -> u32 {
        0x3400_0000
            | (u32::from(is_64_bit) << 31)
            | (u32::from(nonzero) << 24)
            | ((((displacement >> 2) as u32) & 0x7ffff) << 5)
            | u32::from(register)
    }

    fn tb(displacement: i32, nonzero: bool, bit: u8, register: u8) -> u32 {
        0x3600_0000
            | (u32::from(bit >> 5) << 31)
            | (u32::from(nonzero) << 24)
            | (u32::from(bit & 0x1f) << 19)
            | ((((displacement >> 2) as u32) & 0x3fff) << 5)
            | u32::from(register)
    }

    fn render_single(code: &[u8], end: usize, target: RelocationTarget) -> RenderedRelocations {
        let mut capture = RelocationCapture::new(true);
        capture.record_mov_wide(0, end, 16, target);
        capture.render(code).expect("valid relocation")
    }

    #[test]
    fn accepts_one_to_four_chunks_and_skipped_zero_chunks() {
        for chunk_count in 1..=4 {
            let words: Vec<_> = (0..chunk_count)
                .map(|index| {
                    if index == 0 {
                        movz(16, 0x1111, 0, true)
                    } else {
                        movk(16, 0x1111 + index as u16, index as u8 * 16, true)
                    }
                })
                .collect();
            let code = instructions(&words);
            let rendered = render_single(&code, code.len(), runtime_stub());
            let document: Value = serde_json::from_str(&rendered.json).unwrap();
            assert_eq!(
                document["relocations"][0]["chunks"]
                    .as_array()
                    .unwrap()
                    .len(),
                chunk_count
            );
            assert!(!rendered.json.contains("immediate"));
        }

        let code = instructions(&[
            movz(16, 0xaaaa, 0, true),
            movk(16, 0xbbbb, 32, true),
            movk(16, 0xcccc, 48, true),
        ]);
        let rendered = render_single(&code, code.len(), runtime_stub());
        let document: Value = serde_json::from_str(&rendered.json).unwrap();
        let shifts: Vec<_> = document["relocations"][0]["chunks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|chunk| chunk["shiftBits"].as_u64().unwrap())
            .collect();
        assert_eq!(shifts, [0, 32, 48]);
    }

    #[test]
    fn json_is_sorted_uses_code_bin_offsets_and_hides_address_chunks() {
        let code = instructions(&[movz(3, 0xdead, 0, true), NOP, movz(5, 0xbeef, 0, true)]);
        let mut capture = RelocationCapture::new(true);
        capture.record_mov_wide(8, 12, 5, RelocationTarget::GcCageBase);
        capture.record_mov_wide(0, 4, 3, runtime_stub());
        let rendered = capture.render(&code).unwrap();
        let document: Value = serde_json::from_str(&rendered.json).unwrap();
        assert_eq!(document["offsetBasis"], "code.bin");
        assert_eq!(document["addressEncoding"], "symbolicOnly");
        assert_eq!(document["relocations"][0]["startOffset"], 0);
        assert_eq!(document["relocations"][1]["startOffset"], 8);
        assert_eq!(document["relocations"][0]["chunks"][0]["operation"], "movz");
        assert!(
            document["relocations"][0]["chunks"][0]
                .get("immediate")
                .is_none()
        );
        assert!(!rendered.json.contains("57005"));
        assert!(!rendered.json.contains("48879"));
    }

    #[test]
    fn record_order_does_not_change_artifacts() {
        let code = instructions(&[movz(3, 1, 0, true), NOP, movz(5, 2, 0, true)]);
        let mut forward = RelocationCapture::new(true);
        forward.record_mov_wide(0, 4, 3, runtime_stub());
        forward.record_mov_wide(8, 12, 5, RelocationTarget::GcCageBase);
        let mut reverse = RelocationCapture::new(true);
        reverse.record_mov_wide(8, 12, 5, RelocationTarget::GcCageBase);
        reverse.record_mov_wide(0, 4, 3, runtime_stub());
        assert_eq!(forward.render(&code), reverse.render(&code));
    }

    #[test]
    fn disabled_capture_does_not_allocate_or_retain_records() {
        let code = instructions(&[NOP]);
        let mut capture = RelocationCapture::default();
        assert!(!capture.enabled);
        assert_eq!(capture.records.capacity(), 0);
        capture.record_mov_wide(0, 4, 16, runtime_stub());
        assert!(capture.records.is_empty());
        assert_eq!(capture.records.capacity(), 0);

        let rendered = capture.render(&code).unwrap();
        let document: Value = serde_json::from_str(&rendered.json).unwrap();
        assert_eq!(document["relocations"].as_array().unwrap().len(), 0);
        assert!(rendered.normalized_code.starts_with(NORMALIZED_MAGIC));
        assert_eq!(
            &rendered.normalized_code[NORMALIZED_MAGIC.len()..NORMALIZED_MAGIC.len() + 2],
            &NORMALIZED_ARCH_AARCH64.to_le_bytes()
        );
        assert!(rendered.normalized_code.ends_with(&NOP.to_le_bytes()));
    }

    #[test]
    fn runtime_stub_target_uses_stable_abi_names() {
        assert_eq!(
            RelocationTarget::runtime_stub(otter_vm::native_abi::STUB_JIT_BACKEDGE_POLL),
            RelocationTarget::RuntimeStub {
                id: 1,
                name: "jit_backedge_poll",
                signature: "poll1",
            }
        );
        assert_eq!(
            RelocationTarget::runtime_stub(otter_vm::native_abi::STUB_COLLECTION_MAP_GET_LEAF),
            RelocationTarget::RuntimeStub {
                id: 2,
                name: "collection_map_get_leaf",
                signature: "leafValue2",
            }
        );
    }

    #[test]
    fn all_direct_branches_use_logical_item_destinations() {
        fn code_with_relocation(chunks: &[u32]) -> (Vec<u8>, usize, usize) {
            let branch_bytes = 7 * 4;
            let target = branch_bytes + chunks.len() * 4;
            let mut words = Vec::new();
            for source in 0..7 {
                let displacement = target as i32 - source * 4;
                words.push(match source {
                    0 => b(displacement, false),
                    1 => b(displacement, true),
                    2 => b_cond(displacement, 1),
                    3 => cb(displacement, false, false, 3),
                    4 => cb(displacement, true, true, 4),
                    5 => tb(displacement, false, 5, 6),
                    6 => tb(displacement, true, 47, 7),
                    _ => unreachable!(),
                });
            }
            words.extend_from_slice(chunks);
            words.push(RET);
            (instructions(&words), branch_bytes, target)
        }

        let (short_code, short_start, short_end) =
            code_with_relocation(&[movz(16, 0x1111, 0, true)]);
        let (long_code, long_start, long_end) = code_with_relocation(&[
            movz(16, 0xaaaa, 0, true),
            movk(16, 0xbbbb, 16, true),
            movk(16, 0xcccc, 32, true),
            movk(16, 0xdddd, 48, true),
        ]);
        let mut short_capture = RelocationCapture::new(true);
        short_capture.record_mov_wide(short_start, short_end, 16, RelocationTarget::GcCageBase);
        let mut long_capture = RelocationCapture::new(true);
        long_capture.record_mov_wide(long_start, long_end, 16, RelocationTarget::GcCageBase);

        assert_eq!(
            short_capture.render(&short_code).unwrap().normalized_code,
            long_capture.render(&long_code).unwrap().normalized_code
        );
    }

    #[test]
    fn target_semantics_are_explicit_in_normalized_code() {
        let code = instructions(&[movz(16, 0x1234, 0, true)]);
        let targets = [
            RelocationTarget::RuntimeStub {
                id: 1,
                name: "one",
                signature: "poll1",
            },
            RelocationTarget::GcCageBase,
            RelocationTarget::PropertyIcCell {
                access: PropertyIcAccess::Load,
                ordinal: 2,
            },
            RelocationTarget::TemplateOperandSlice {
                arena: TemplateOperandArena::Registers,
                role: TemplateOperandRole::ConstructArguments,
                start: 3,
                len: 4,
            },
            RelocationTarget::OptimizedMathArguments {
                inline_frame: 5,
                logical_pc: 6,
                len: 7,
            },
            RelocationTarget::GuardedHeapReference {
                component: GuardedHeapComponent::Prototype,
                feedback_kind: GuardedBuiltinKind::Leaf,
                byte_pc: 8,
                runtime_stub_id: 9,
            },
            RelocationTarget::GuardedBuiltinFunction {
                feedback_kind: GuardedBuiltinKind::Alloc,
                byte_pc: 10,
                runtime_stub_id: 11,
            },
            RelocationTarget::StaticNativeBuiltinFunction {
                target: otter_vm::JitStaticNativeCallKind::MathAbs,
                byte_pc: 12,
            },
            direct_call_target(
                29,
                DirectCallTierArtifact::Optimizing,
                DirectCallThisModeArtifact::SloppyGlobal,
            ),
        ];
        let normalized: BTreeSet<_> = targets
            .into_iter()
            .map(|target| render_single(&code, 4, target).normalized_code)
            .collect();
        assert_eq!(normalized.len(), 9);

        let first = render_single(
            &code,
            4,
            RelocationTarget::RuntimeStub {
                id: 1,
                name: "same",
                signature: "poll1",
            },
        );
        let second = render_single(
            &code,
            4,
            RelocationTarget::RuntimeStub {
                id: 2,
                name: "same",
                signature: "poll1",
            },
        );
        assert_ne!(first.normalized_code, second.normalized_code);
        assert!(first.normalized_code.starts_with(NORMALIZED_MAGIC));
    }

    #[test]
    fn direct_call_exact_generation_is_not_portable_normalized_identity() {
        let code = instructions(&[movz(16, 0x1234, 0, true)]);
        let first = render_single(
            &code,
            4,
            direct_call_target(
                29,
                DirectCallTierArtifact::Optimizing,
                DirectCallThisModeArtifact::SloppyGlobal,
            ),
        );
        let second = render_single(
            &code,
            4,
            direct_call_target(
                30,
                DirectCallTierArtifact::Optimizing,
                DirectCallThisModeArtifact::SloppyGlobal,
            ),
        );
        assert_eq!(first.normalized_code, second.normalized_code);
        assert_ne!(first.json, second.json);

        let document: Value = serde_json::from_str(&first.json).unwrap();
        let target = &document["relocations"][0]["target"];
        assert_eq!(target["kind"], "directCallEntryCell");
        assert_eq!(target["bytePc"], 13);
        assert_eq!(target["directCall"]["targetFunctionId"], 12);
        assert_eq!(target["directCall"]["targetCodeObjectId"], 29);
        assert_eq!(target["directCall"]["targetTier"], "optimizing");
        assert_eq!(target["directCall"]["thisMode"], "sloppyGlobal");
        assert_eq!(target["directCall"]["calleeNativeFrameBytes"], 160);
        assert_eq!(target["directCall"]["linkageBytes"], 112);
        assert_eq!(target["directCall"]["reservedStackBytes"], 272);
        assert_eq!(target["directCall"]["calleeRegisterCount"], 6);

        let template = render_single(
            &code,
            4,
            direct_call_target(
                29,
                DirectCallTierArtifact::Template,
                DirectCallThisModeArtifact::SloppyGlobal,
            ),
        );
        assert_ne!(first.normalized_code, template.normalized_code);

        let strict_or_lexical = render_single(
            &code,
            4,
            direct_call_target(
                29,
                DirectCallTierArtifact::Optimizing,
                DirectCallThisModeArtifact::StrictOrLexical,
            ),
        );
        assert_ne!(first.normalized_code, strict_or_lexical.normalized_code);
    }

    #[test]
    fn rejects_malformed_ranges_and_mov_wide_sequences() {
        let code = instructions(&[
            movz(16, 1, 0, true),
            movk(16, 2, 16, true),
            movk(16, 3, 32, true),
            movk(16, 4, 48, true),
            NOP,
        ]);

        let error_for = |start, end, register| {
            let mut capture = RelocationCapture::new(true);
            capture.record_mov_wide(start, end, register, runtime_stub());
            capture.render(&code).unwrap_err()
        };
        assert_eq!(
            error_for(0, 0, 16),
            RelocationError::EmptyRange { start: 0 }
        );
        assert_eq!(
            error_for(1, 4, 16),
            RelocationError::RangeNotInstructionAligned { start: 1, end: 4 }
        );
        assert_eq!(
            error_for(0, 24, 16),
            RelocationError::RangeOutOfBounds {
                start: 0,
                end: 24,
                code_len: 20
            }
        );
        assert_eq!(
            error_for(0, 20, 16),
            RelocationError::MovWideInstructionCount { start: 0, count: 5 }
        );
        assert_eq!(
            error_for(0, 4, 32),
            RelocationError::InvalidRegister {
                start: 0,
                register: 32
            }
        );

        let malformed = [
            (
                instructions(&[NOP]),
                RelocationError::ExpectedMovz { offset: 0 },
            ),
            (
                instructions(&[movz(16, 1, 0, true), movz(16, 2, 16, true)]),
                RelocationError::ExpectedMovk { offset: 4 },
            ),
            (
                instructions(&[movz(15, 1, 0, true)]),
                RelocationError::RegisterMismatch {
                    offset: 0,
                    expected: 16,
                    actual: 15,
                },
            ),
            (
                instructions(&[movz(16, 1, 0, true), movk(16, 2, 16, false)]),
                RelocationError::WidthMismatch {
                    offset: 4,
                    expected_bits: 64,
                    actual_bits: 32,
                },
            ),
            (
                instructions(&[movz(16, 1, 16, true)]),
                RelocationError::FirstWideShiftNotZero {
                    offset: 0,
                    shift_bits: 16,
                },
            ),
            (
                instructions(&[movz(16, 1, 0, false), movk(16, 2, 32, false)]),
                RelocationError::InvalidWideShift {
                    offset: 4,
                    width_bits: 32,
                    shift_bits: 32,
                },
            ),
            (
                instructions(&[
                    movz(16, 1, 0, true),
                    movk(16, 2, 16, true),
                    movk(16, 3, 16, true),
                ]),
                RelocationError::NonIncreasingWideShift {
                    offset: 8,
                    previous_shift_bits: 16,
                    shift_bits: 16,
                },
            ),
        ];
        for (malformed_code, expected) in malformed {
            let mut capture = RelocationCapture::new(true);
            capture.record_mov_wide(0, malformed_code.len(), 16, runtime_stub());
            assert_eq!(capture.render(&malformed_code).unwrap_err(), expected);
        }
    }

    #[test]
    fn rejects_overlap_before_decoding_ranges() {
        let code = instructions(&[movz(16, 1, 0, true), movk(16, 2, 16, true)]);
        let mut capture = RelocationCapture::new(true);
        capture.record_mov_wide(0, 8, 16, runtime_stub());
        capture.record_mov_wide(4, 8, 16, RelocationTarget::GcCageBase);
        assert_eq!(
            capture.render(&code).unwrap_err(),
            RelocationError::OverlappingRanges {
                previous_start: 0,
                previous_end: 8,
                start: 4,
                end: 8,
            }
        );
    }

    #[test]
    fn rejects_unsupported_pc_relative_instructions() {
        for (instruction, name) in [
            (0x1000_0000, "ADR"),
            (0x9000_0000, "ADRP"),
            (0x5800_0000, "literal load/prefetch"),
            (0xd800_0000, "literal load/prefetch"),
            (0x5400_0010, "BC.cond"),
        ] {
            let code = instructions(&[instruction]);
            assert_eq!(
                RelocationCapture::default().render(&code).unwrap_err(),
                RelocationError::UnsupportedPcRelative {
                    offset: 0,
                    instruction: name,
                }
            );
        }
    }

    #[test]
    fn rejects_branches_into_relocation_interiors_and_outside_code() {
        let code = instructions(&[
            b(8, false),
            movz(16, 1, 0, true),
            movk(16, 2, 16, true),
            RET,
        ]);
        let mut capture = RelocationCapture::new(true);
        capture.record_mov_wide(4, 12, 16, runtime_stub());
        assert_eq!(
            capture.render(&code).unwrap_err(),
            RelocationError::BranchIntoRelocation {
                offset: 0,
                target: 8,
                relocation_start: 4,
                relocation_end: 12,
            }
        );

        let outside = instructions(&[b(-4, false)]);
        assert_eq!(
            RelocationCapture::default().render(&outside).unwrap_err(),
            RelocationError::BranchTargetOutOfBounds {
                offset: 0,
                target: -4,
                code_len: 4,
            }
        );

        let to_end = instructions(&[b(4, false)]);
        assert_eq!(
            RelocationCapture::default().render(&to_end).unwrap_err(),
            RelocationError::BranchTargetOutOfBounds {
                offset: 0,
                target: 4,
                code_len: 4,
            }
        );
    }

    #[test]
    fn rejects_non_instruction_sized_code() {
        assert_eq!(
            RelocationCapture::default().render(&[0, 1, 2]).unwrap_err(),
            RelocationError::CodeLengthNotInstructionAligned { code_len: 3 }
        );
    }
}
