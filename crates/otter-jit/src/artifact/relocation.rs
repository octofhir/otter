//! Address-free native relocation artifacts and semantic code normalization.
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
//!   the target's fixed address-materialization form: AArch64 `MOVZ`/`MOVK`
//!   sequences, an 8-byte AArch64 literal-pool word that one PC-relative
//!   `LDR (literal)` reads (V8's arm64 constant pool), or one x86-64
//!   `mov r64, imm64`.
//! - A literal pool follows every instruction of its code object, which the
//!   tier keeps within the `LDR (literal)` range; each distinct address has
//!   one word.
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

const NORMALIZED_MAGIC: &[u8; 8] = b"OTJNCODE";
#[cfg(not(target_arch = "x86_64"))]
const NORMALIZED_ARCH_AARCH64: u16 = 1;
#[cfg(target_arch = "x86_64")]
const NORMALIZED_ARCH_X86_64: u16 = 2;

#[cfg(not(target_arch = "x86_64"))]
const ITEM_RAW_INSTRUCTION: u8 = 0;
const ITEM_RELOCATION: u8 = 1;
#[cfg(not(target_arch = "x86_64"))]
const ITEM_DIRECT_BRANCH: u8 = 2;
#[cfg(not(target_arch = "x86_64"))]
const ITEM_DATA_WORD: u8 = 4;
#[cfg(not(target_arch = "x86_64"))]
const ITEM_LITERAL_WORD: u8 = 5;

const TARGET_RUNTIME_STUB: u8 = 1;
const TARGET_GC_CAGE_BASE: u8 = 2;
const TARGET_PROPERTY_IC_SLOT: u8 = 3;
const TARGET_GUARDED_HEAP_REFERENCE: u8 = 6;
const TARGET_FUNCTION_ENTRY_CELL: u8 = 8;
const TARGET_GLOBAL_LEXICAL_CELL: u8 = 10;
const TARGET_DEOPT_RUNTIME_DATA: u8 = 11;
const TARGET_LITERAL_CELL: u8 = 12;
const TARGET_PROPERTY_ACTION_CACHE_TABLE: u8 = 13;
const TARGET_CALLEE_IDENTITY_CELL: u8 = 16;
const TARGET_ARITH_FEEDBACK_CELL: u8 = 17;
const TARGET_SOURCE_WORK_CELL: u8 = 18;
const TARGET_INSTANCEOF_CELL: u8 = 19;
const TARGET_PROTOTYPE_VALIDITY_CELL: u8 = 15;

/// Whether a named-property probe serves a load or a store site.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum PropertySourceAccess {
    Load,
    Store,
}

/// Address-stable heap component used by a collection fast path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(target_arch = "x86_64", allow(dead_code))]
pub(crate) enum GuardedHeapComponent {
    Prototype,
    PrototypeShape,
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
#[cfg_attr(target_arch = "x86_64", allow(dead_code))]
pub(crate) enum RelocationTarget {
    RuntimeStub {
        id: u32,
        name: &'static str,
        signature: &'static str,
    },
    GcCageBase,
    /// The isolate's one property action table with independent load/store facts.
    /// Its traced roots stay live; the relocation never embeds a cached GC value.
    PropertyActionCacheTable,
    /// A retained ordinary-chain validity word, identified by its root shape.
    PrototypeValidityCell {
        identity: u64,
    },
    /// The code object's [`otter_vm::deopt::DeoptRuntime`] allocation, read by
    /// the shared deopt handler. The process address is deliberately absent;
    /// the code object owns exactly one.
    DeoptRuntimeData,
    /// Permanent global-declarative cell read by one `LoadGlobalOrThrow`.
    ///
    /// A byte PC names an instruction only inside its own body, so a spliced
    /// frame's read is identified by that body's function id as well.
    GlobalLexicalCell {
        function_id: u32,
        byte_pc: u32,
    },
    /// Address-stable GC-traced cell for one eagerly prepared string or BigInt
    /// literal.
    LiteralCell {
        function_id: u32,
        byte_pc: u32,
    },
    /// The native IC slot of one named-property site, owned by the site's
    /// CodeBlock feedback vector. A byte PC names an instruction only inside
    /// its own body, so the function id is part of the identity.
    PropertyIcSlot {
        function_id: u32,
        byte_pc: u32,
    },
    GuardedHeapReference {
        component: GuardedHeapComponent,
        byte_pc: u32,
        runtime_stub_id: u32,
    },
    /// Permanent `FunctionEntryCell` of a proven call target, through which
    /// a generated call enters the target's current generation. The process
    /// address is absent; the function id is the portable identity.
    FunctionEntryCell {
        function_id: u32,
    },
    /// Code-owned cache of the last callee one call site proved to be its
    /// baked target, named by that target and the site's canonical PC.
    CalleeIdentityCell {
        function_id: u32,
        call_pc: u32,
    },
    /// Code-owned cache of the last target and prototype one `instanceof`
    /// site proved, named by the site's function and byte PC.
    InstanceofCell {
        function_id: u32,
        byte_pc: u32,
    },
    /// Canonical non-GC opcode-work scalar retained by emitted Template code.
    SourceWorkCell {
        function_id: u32,
    },
    /// Live arithmetic observation byte of one instruction, which baseline
    /// code records into.
    ArithFeedbackCell {
        function_id: u32,
        pc: u32,
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
        RuntimeStubSignature::Float64Leaf2 => "float64Leaf2",
        RuntimeStubSignature::Float64ToWordLeaf1 => "float64ToWordLeaf1",
        RuntimeStubSignature::AllocValue3 => "allocValue3",
        RuntimeStubSignature::Poll1 => "poll1",
        RuntimeStubSignature::Variadic => "variadic",
        RuntimeStubSignature::ContextWords => "contextWords",
        RuntimeStubSignature::MutatingLeafValue2 => "mutatingLeafValue2",
        RuntimeStubSignature::MutatingLeafValue3 => "mutatingLeafValue3",
        RuntimeStubSignature::ReentrantValue2 => "reentrantValue2",
        RuntimeStubSignature::ReentrantValue3 => "reentrantValue3",
        RuntimeStubSignature::ReentrantNamedLoad => "reentrantNamedLoad",
        RuntimeStubSignature::ReentrantNamedStore => "reentrantNamedStore",
        RuntimeStubSignature::ReentrantValueSpan => "reentrantValueSpan",
        RuntimeStubSignature::CommittedValue2 => "committedValue2",
        RuntimeStubSignature::RouteThrow1 => "routeThrow1",
        RuntimeStubSignature::ExecutionEntry0 => "executionEntry0",
        RuntimeStubSignature::JsCall => "jsCall",
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RelocationRecord {
    start: usize,
    end: usize,
    register: u8,
    target: RelocationTarget,
    form: RelocationForm,
}

/// How a relocation site encodes its address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) enum RelocationForm {
    /// `MOVZ`/`MOVK` instructions, or x86-64 `mov r64, imm64`.
    Immediate,
    /// One AArch64 `LDR (literal)` reading the target's pool word.
    LiteralLoad,
    /// The 8-byte pool word holding the target's address.
    LiteralWord,
}

impl RelocationForm {
    fn is_immediate(&self) -> bool {
        *self == Self::Immediate
    }
}

/// The pending AArch64 literal pool of one code object: each distinct
/// symbolic address once, with the label its loads address.
#[cfg(not(target_arch = "x86_64"))]
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct LiteralPool {
    enabled: bool,
    entries: Vec<(u64, RelocationTarget, dynasmrt::DynamicLabel)>,
    index: rustc_hash::FxHashMap<u64, usize>,
}

/// Optional emission-side typed relocation storage, and the literal pool
/// address loads may use.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RelocationCapture {
    enabled: bool,
    records: Vec<RelocationRecord>,
    /// Byte ranges of in-code data (literal pools), never instructions.
    data: Vec<(usize, usize)>,
    #[cfg(not(target_arch = "x86_64"))]
    pool: LiteralPool,
}

impl RelocationCapture {
    /// Creates capture storage without allocating a record buffer.
    pub(crate) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            records: Vec::new(),
            data: Vec::new(),
            #[cfg(not(target_arch = "x86_64"))]
            pool: LiteralPool::default(),
        }
    }

    /// Load every symbolic address through one PC-relative `LDR (literal)`
    /// of a pool word: the caller emits [`Self::emit_literal_pool`] after
    /// its last instruction and keeps the code within the instruction's
    /// ±1 MiB range.
    #[cfg(not(target_arch = "x86_64"))]
    pub(crate) fn with_literal_pool(mut self) -> Self {
        self.pool.enabled = true;
        self
    }

    /// Load `value` into `X(register)` with one `LDR (literal)` of its pool
    /// word, when the pool is enabled; `false` leaves the load to the
    /// caller.
    #[cfg(not(target_arch = "x86_64"))]
    pub(crate) fn emit_literal_load(
        &mut self,
        ops: &mut dynasmrt::aarch64::Assembler,
        register: u8,
        value: u64,
        target: RelocationTarget,
    ) -> bool {
        use dynasmrt::{DynasmApi, DynasmLabelApi, dynasm};
        if !self.pool.enabled {
            return false;
        }
        let label = if let Some(&index) = self.pool.index.get(&value) {
            self.pool.entries[index].2
        } else {
            let label = ops.new_dynamic_label();
            self.pool.index.insert(value, self.pool.entries.len());
            self.pool.entries.push((value, target.clone(), label));
            label
        };
        let start = ops.offset().0;
        dynasm!(ops ; .arch aarch64 ; ldr X(register), =>label);
        if self.enabled {
            self.records.push(RelocationRecord {
                start,
                end: start + 4,
                register,
                target,
                form: RelocationForm::LiteralLoad,
            });
        }
        true
    }

    /// Emit the pool every [`Self::emit_literal_load`] reads, after the code
    /// object's last instruction; the pool's byte range, if any.
    #[cfg(not(target_arch = "x86_64"))]
    pub(crate) fn emit_literal_pool(
        &mut self,
        ops: &mut dynasmrt::aarch64::Assembler,
    ) -> Option<(usize, usize)> {
        use dynasmrt::{DynasmApi, DynasmLabelApi, dynasm};
        if self.pool.entries.is_empty() {
            return None;
        }
        let pool_start = ops.offset().0;
        dynasm!(ops ; .arch aarch64 ; .align 8);
        for (value, target, label) in std::mem::take(&mut self.pool.entries) {
            let start = ops.offset().0;
            dynasm!(ops ; .arch aarch64 ; =>label ; .u64 value);
            if self.enabled {
                self.records.push(RelocationRecord {
                    start,
                    end: start + 8,
                    register: 0,
                    target,
                    form: RelocationForm::LiteralWord,
                });
            }
        }
        self.pool.index.clear();
        Some((pool_start, ops.offset().0))
    }

    /// Records one in-code data range (a literal pool) that `ldr (literal)`
    /// instructions address.
    #[cfg(not(target_arch = "x86_64"))]
    pub(crate) fn record_data(&mut self, start: usize, end: usize) {
        if self.enabled {
            self.data.push((start, end));
        }
    }

    /// Records one emitted MOV-wide address materialization.
    ///
    /// Validation is intentionally deferred until finalized code is available:
    /// `render` verifies both the byte range and every encoded instruction.
    #[cfg(not(target_arch = "x86_64"))]
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
            form: RelocationForm::Immediate,
        });
    }

    /// Records one fixed-width x86-64 `mov r64, imm64` materialization.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn record_x86_imm64(
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
            form: RelocationForm::Immediate,
        });
    }

    /// Renders address-free relocation metadata and portable semantic code.
    pub(crate) fn render(&self, code: &[u8]) -> Result<RenderedRelocations, RelocationError> {
        #[cfg(target_arch = "x86_64")]
        {
            render_x86_64(&self.records, code)
        }
        #[cfg(not(target_arch = "x86_64"))]
        {
            if !code.len().is_multiple_of(4) {
                return Err(RelocationError::CodeLengthNotInstructionAligned {
                    code_len: code.len(),
                });
            }

            let validated = ValidatedRelocations {
                records: validate_relocations(&self.records, code)?,
            };
            let logical_items = build_logical_items(&validated.records, &self.data, code);
            let normalized_code = render_normalized(&validated.records, &logical_items, code)?;
            let json = render_json(&validated.records);
            Ok(RenderedRelocations {
                json,
                normalized_code,
                validated,
            })
        }
    }
}

#[cfg(target_arch = "x86_64")]
fn render_x86_64(
    records: &[RelocationRecord],
    code: &[u8],
) -> Result<RenderedRelocations, RelocationError> {
    let mut records = records.to_vec();
    records.sort_by_key(|record| (record.start, record.end, record.register));
    let mut previous_start = 0usize;
    let mut previous_end = 0usize;
    let mut validated = Vec::with_capacity(records.len());
    for record in records {
        if record.start == record.end {
            return Err(RelocationError::EmptyRange {
                start: record.start,
            });
        }
        if record.start < previous_end {
            return Err(RelocationError::OverlappingRanges {
                previous_start,
                previous_end,
                start: record.start,
                end: record.end,
            });
        }
        if record.end > code.len() || record.start > record.end {
            return Err(RelocationError::RangeOutOfBounds {
                start: record.start,
                end: record.end,
                code_len: code.len(),
            });
        }
        if record.register > 15 {
            return Err(RelocationError::InvalidRegister {
                start: record.start,
                register: record.register,
            });
        }
        let bytes = &code[record.start..record.end];
        let expected_rex = 0x48 | u8::from(record.register >= 8);
        let expected_opcode = 0xb8 | (record.register & 7);
        if bytes.len() != 10 || bytes[0] != expected_rex || bytes[1] != expected_opcode {
            return Err(RelocationError::ExpectedX86Imm64Move {
                start: record.start,
                end: record.end,
                register: record.register,
            });
        }
        previous_start = record.start;
        previous_end = record.end;
        validated.push(ValidatedRelocation {
            start_offset: record.start as u64,
            end_offset: record.end as u64,
            register: record.register,
            width_bits: 64,
            chunks: Vec::new(),
            target: record.target,
            form: RelocationForm::Immediate,
        });
    }
    let json = render_json(&validated);
    let normalized_code = render_x86_64_normalized(&validated, code)?;
    Ok(RenderedRelocations {
        json,
        normalized_code,
        validated: ValidatedRelocations { records: validated },
    })
}

#[cfg(target_arch = "x86_64")]
fn render_x86_64_normalized(
    relocations: &[ValidatedRelocation],
    code: &[u8],
) -> Result<Vec<u8>, RelocationError> {
    let raw_chunks = relocations.len().saturating_add(1);
    let item_count = relocations.len().checked_add(raw_chunks).ok_or(
        RelocationError::LogicalItemCountOverflow {
            count: relocations.len(),
        },
    )?;
    let item_count = u32::try_from(item_count)
        .map_err(|_| RelocationError::LogicalItemCountOverflow { count: item_count })?;
    let mut output = Vec::with_capacity(code.len().saturating_add(relocations.len() * 16));
    output.extend_from_slice(NORMALIZED_MAGIC);
    put_u16(&mut output, NORMALIZED_ARCH_X86_64);
    put_u32(&mut output, item_count);
    let mut offset = 0usize;
    for relocation in relocations {
        encode_raw_bytes(&mut output, &code[offset..relocation.start()])?;
        output.push(ITEM_RELOCATION);
        output.push(relocation.register);
        output.push(relocation.width_bits);
        encode_target(&relocation.target, &mut output)?;
        offset = relocation.end();
    }
    encode_raw_bytes(&mut output, &code[offset..])?;
    Ok(output)
}

#[cfg(target_arch = "x86_64")]
fn encode_raw_bytes(output: &mut Vec<u8>, bytes: &[u8]) -> Result<(), RelocationError> {
    const ITEM_RAW_BYTES: u8 = 3;
    let len = u32::try_from(bytes.len())
        .map_err(|_| RelocationError::LogicalItemCountOverflow { count: bytes.len() })?;
    output.push(ITEM_RAW_BYTES);
    put_u32(output, len);
    output.extend_from_slice(bytes);
    Ok(())
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
#[cfg_attr(target_arch = "x86_64", allow(dead_code))]
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
    #[cfg(target_arch = "x86_64")]
    ExpectedX86Imm64Move {
        start: usize,
        end: usize,
        register: u8,
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
            #[cfg(target_arch = "x86_64")]
            Self::ExpectedX86Imm64Move {
                start,
                end,
                register,
            } => write!(
                formatter,
                "expected x86-64 mov r{register}, imm64 at byte range {start}..{end}"
            ),
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
#[cfg_attr(target_arch = "x86_64", allow(dead_code))]
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
    #[serde(skip_serializing_if = "RelocationForm::is_immediate")]
    pub(super) form: RelocationForm,
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
#[cfg(not(target_arch = "x86_64"))]
enum LogicalItem {
    RawInstruction {
        offset: usize,
    },
    Relocation {
        index: usize,
    },
    #[cfg(not(target_arch = "x86_64"))]
    DataWord {
        offset: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(not(target_arch = "x86_64"))]
enum MovWideKind {
    Movz,
    Movk,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(not(target_arch = "x86_64"))]
struct DecodedMovWide {
    kind: MovWideKind,
    register: u8,
    width_bits: u8,
    shift_bits: u8,
}

#[cfg(not(target_arch = "x86_64"))]
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
        .map(|record| match record.form {
            RelocationForm::Immediate => validate_mov_wide(record, code),
            RelocationForm::LiteralLoad => {
                let instruction = read_instruction(code, record.start);
                if instruction & 0xff00_0000 != 0x5800_0000
                    || (instruction & 0x1f) as u8 != record.register
                {
                    return Err(RelocationError::ExpectedMovz {
                        offset: record.start,
                    });
                }
                Ok(ValidatedRelocation {
                    start_offset: record.start as u64,
                    end_offset: record.end as u64,
                    register: record.register,
                    width_bits: 64,
                    chunks: Vec::new(),
                    target: record.target,
                    form: record.form,
                })
            }
            RelocationForm::LiteralWord => Ok(ValidatedRelocation {
                start_offset: record.start as u64,
                end_offset: record.end as u64,
                register: record.register,
                width_bits: 64,
                chunks: Vec::new(),
                target: record.target,
                form: record.form,
            }),
        })
        .collect()
}

#[cfg(not(target_arch = "x86_64"))]
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
    if record.form == RelocationForm::LiteralLoad {
        if instruction_count != 1 {
            return Err(RelocationError::MovWideInstructionCount {
                start: record.start,
                count: instruction_count,
            });
        }
    } else if record.form == RelocationForm::LiteralWord {
        if instruction_count != 2 || !record.start.is_multiple_of(8) {
            return Err(RelocationError::RangeNotInstructionAligned {
                start: record.start,
                end: record.end,
            });
        }
    } else if !(1..=4).contains(&instruction_count) {
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

#[cfg(not(target_arch = "x86_64"))]
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
        form: RelocationForm::Immediate,
    })
}

#[cfg(not(target_arch = "x86_64"))]
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

#[cfg(not(target_arch = "x86_64"))]
fn build_logical_items(
    relocations: &[ValidatedRelocation],
    data: &[(usize, usize)],
    code: &[u8],
) -> Vec<LogicalItem> {
    let mut items = Vec::new();
    let mut offset = 0;
    let mut relocation_index = 0;
    while offset < code.len() {
        if data
            .iter()
            .any(|&(start, end)| (start..end).contains(&offset))
        {
            items.push(LogicalItem::DataWord { offset });
            offset += 4;
        } else if relocation_index < relocations.len()
            && relocations[relocation_index].start() == offset
        {
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

#[cfg(not(target_arch = "x86_64"))]
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
            LogicalItem::RawInstruction { offset } | LogicalItem::DataWord { offset } => *offset,
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
                if relocation.form == RelocationForm::LiteralWord {
                    output.push(ITEM_LITERAL_WORD);
                } else {
                    output.push(ITEM_RELOCATION);
                    output.push(relocation.register);
                    output.push(relocation.width_bits);
                }
                encode_target(&relocation.target, &mut output)?;
            }
            LogicalItem::DataWord { offset } => {
                output.push(ITEM_DATA_WORD);
                output.extend_from_slice(&code[*offset..*offset + 4]);
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

#[cfg(not(target_arch = "x86_64"))]
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
#[cfg(not(target_arch = "x86_64"))]
pub(super) enum DirectBranchKind {
    B,
    Bl,
    BCond {
        condition: u8,
    },
    Cbz {
        is_64_bit: bool,
        register: u8,
    },
    Cbnz {
        is_64_bit: bool,
        register: u8,
    },
    Tbz {
        bit: u8,
        register: u8,
    },
    Tbnz {
        bit: u8,
        register: u8,
    },
    /// Code-relative address materialization (`adr`).
    Adr {
        register: u8,
    },
    /// Code-relative literal load or prefetch (`ldr (literal)`): `opc`
    /// and the vector bit name the access, `register` its target.
    LoadLiteral {
        opc: u8,
        vector: bool,
        register: u8,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(not(target_arch = "x86_64"))]
pub(super) struct DirectBranch {
    pub(super) kind: DirectBranchKind,
    pub(super) target: i64,
}

#[cfg(not(target_arch = "x86_64"))]
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
            DirectBranchKind::Adr { register } => {
                output.push(7);
                output.push(register);
            }
            DirectBranchKind::LoadLiteral {
                opc,
                vector,
                register,
            } => {
                output.push(8);
                output.push(opc);
                output.push(u8::from(vector));
                output.push(register);
            }
        }
    }
}

#[cfg(not(target_arch = "x86_64"))]
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
    if instruction & 0x9f00_0000 == 0x1000_0000 {
        let immediate = ((instruction >> 5) & 0x7ffff) << 2 | ((instruction >> 29) & 0x3);
        return Some(DirectBranch {
            kind: DirectBranchKind::Adr {
                register: (instruction & 0x1f) as u8,
            },
            target: offset as i64 + sign_extend(immediate, 21),
        });
    }
    if instruction & 0x3b00_0000 == 0x1800_0000 {
        return Some(DirectBranch {
            kind: DirectBranchKind::LoadLiteral {
                opc: (instruction >> 30) as u8,
                vector: instruction & (1 << 26) != 0,
                register: (instruction & 0x1f) as u8,
            },
            target: offset as i64 + (sign_extend((instruction >> 5) & 0x7ffff, 19) << 2),
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

#[cfg(not(target_arch = "x86_64"))]
fn unsupported_pc_relative(instruction: u32) -> Option<&'static str> {
    if instruction & 0x9f00_0000 == 0x9000_0000 {
        return Some("ADRP");
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
        RelocationTarget::PropertyActionCacheTable => {
            output.push(TARGET_PROPERTY_ACTION_CACHE_TABLE)
        }
        RelocationTarget::PrototypeValidityCell { identity } => {
            output.push(TARGET_PROTOTYPE_VALIDITY_CELL);
            output.extend_from_slice(&identity.to_le_bytes());
        }
        RelocationTarget::DeoptRuntimeData => output.push(TARGET_DEOPT_RUNTIME_DATA),
        RelocationTarget::GlobalLexicalCell {
            function_id,
            byte_pc,
        } => {
            output.push(TARGET_GLOBAL_LEXICAL_CELL);
            put_u32(output, *function_id);
            put_u32(output, *byte_pc);
        }
        RelocationTarget::LiteralCell {
            function_id,
            byte_pc,
        } => {
            output.push(TARGET_LITERAL_CELL);
            put_u32(output, *function_id);
            put_u32(output, *byte_pc);
        }
        RelocationTarget::PropertyIcSlot {
            function_id,
            byte_pc,
        } => {
            output.push(TARGET_PROPERTY_IC_SLOT);
            put_u32(output, *function_id);
            put_u32(output, *byte_pc);
        }
        RelocationTarget::GuardedHeapReference {
            component,
            byte_pc,
            runtime_stub_id,
        } => {
            output.push(TARGET_GUARDED_HEAP_REFERENCE);
            output.push(match component {
                GuardedHeapComponent::Prototype => 0,
                GuardedHeapComponent::PrototypeShape => 1,
            });
            put_u32(output, *byte_pc);
            put_u32(output, *runtime_stub_id);
        }
        RelocationTarget::FunctionEntryCell { function_id } => {
            output.push(TARGET_FUNCTION_ENTRY_CELL);
            put_u32(output, *function_id);
        }
        RelocationTarget::CalleeIdentityCell {
            function_id,
            call_pc,
        } => {
            output.push(TARGET_CALLEE_IDENTITY_CELL);
            put_u32(output, *function_id);
            put_u32(output, *call_pc);
        }
        RelocationTarget::InstanceofCell {
            function_id,
            byte_pc,
        } => {
            output.push(TARGET_INSTANCEOF_CELL);
            put_u32(output, *function_id);
            put_u32(output, *byte_pc);
        }
        RelocationTarget::SourceWorkCell { function_id } => {
            output.push(TARGET_SOURCE_WORK_CELL);
            put_u32(output, *function_id);
        }
        RelocationTarget::ArithFeedbackCell { function_id, pc } => {
            output.push(TARGET_ARITH_FEEDBACK_CELL);
            put_u32(output, *function_id);
            put_u32(output, *pc);
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

#[cfg(not(target_arch = "x86_64"))]
fn read_instruction(code: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        code[offset..offset + 4]
            .try_into()
            .expect("validated AArch64 instruction range"),
    )
}

#[cfg(not(target_arch = "x86_64"))]
fn sign_extend(value: u32, bits: u32) -> i64 {
    let shift = 64 - bits;
    (i64::from(value) << shift) >> shift
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use serde_json::Value;

    use super::*;

    #[cfg(target_arch = "aarch64")]
    const NOP: u32 = 0xd503_201f;
    #[cfg(target_arch = "aarch64")]
    const RET: u32 = 0xd65f_03c0;

    fn runtime_stub() -> RelocationTarget {
        RelocationTarget::runtime_stub(otter_vm::native_abi::STUB_JIT_BACKEDGE_POLL)
    }

    fn function_entry_cell(function_id: u32) -> RelocationTarget {
        RelocationTarget::FunctionEntryCell { function_id }
    }

    #[cfg(target_arch = "aarch64")]
    fn instructions(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|word| word.to_le_bytes()).collect()
    }

    #[cfg(target_arch = "aarch64")]
    fn movz(register: u8, immediate: u16, shift_bits: u8, is_64_bit: bool) -> u32 {
        let base = if is_64_bit { 0xd280_0000 } else { 0x5280_0000 };
        base | (u32::from(shift_bits / 16) << 21)
            | (u32::from(immediate) << 5)
            | u32::from(register)
    }

    #[cfg(target_arch = "aarch64")]
    fn movk(register: u8, immediate: u16, shift_bits: u8, is_64_bit: bool) -> u32 {
        let base = if is_64_bit { 0xf280_0000 } else { 0x7280_0000 };
        base | (u32::from(shift_bits / 16) << 21)
            | (u32::from(immediate) << 5)
            | u32::from(register)
    }

    #[cfg(target_arch = "aarch64")]
    fn b(displacement: i32, link: bool) -> u32 {
        let immediate = ((displacement >> 2) as u32) & 0x03ff_ffff;
        (if link { 0x9400_0000 } else { 0x1400_0000 }) | immediate
    }

    #[cfg(target_arch = "aarch64")]
    fn b_cond(displacement: i32, condition: u8) -> u32 {
        0x5400_0000 | ((((displacement >> 2) as u32) & 0x7ffff) << 5) | u32::from(condition)
    }

    #[cfg(target_arch = "aarch64")]
    fn cb(displacement: i32, nonzero: bool, is_64_bit: bool, register: u8) -> u32 {
        0x3400_0000
            | (u32::from(is_64_bit) << 31)
            | (u32::from(nonzero) << 24)
            | ((((displacement >> 2) as u32) & 0x7ffff) << 5)
            | u32::from(register)
    }

    #[cfg(target_arch = "aarch64")]
    fn tb(displacement: i32, nonzero: bool, bit: u8, register: u8) -> u32 {
        0x3600_0000
            | (u32::from(bit >> 5) << 31)
            | (u32::from(nonzero) << 24)
            | (u32::from(bit & 0x1f) << 19)
            | ((((displacement >> 2) as u32) & 0x3fff) << 5)
            | u32::from(register)
    }

    #[cfg(target_arch = "aarch64")]
    fn render_single(code: &[u8], end: usize, target: RelocationTarget) -> RenderedRelocations {
        let mut capture = RelocationCapture::new(true);
        capture.record_mov_wide(0, end, 16, target);
        capture.render(code).expect("valid relocation")
    }

    #[cfg(target_arch = "x86_64")]
    fn x86_mov(register: u8, immediate: u64) -> Vec<u8> {
        let mut code = vec![0x48 | u8::from(register >= 8), 0xb8 | (register & 7)];
        code.extend_from_slice(&immediate.to_le_bytes());
        code
    }

    #[cfg(target_arch = "x86_64")]
    fn render_x86_single(
        register: u8,
        immediate: u64,
        target: RelocationTarget,
    ) -> RenderedRelocations {
        let code = x86_mov(register, immediate);
        let mut capture = RelocationCapture::new(true);
        capture.record_x86_imm64(0, code.len(), register, target);
        capture.render(&code).expect("valid x86-64 relocation")
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn x86_imm64_relocations_validate_sort_and_hide_addresses() {
        let first = x86_mov(3, 0xfeed_face_dead_beef);
        let second = x86_mov(12, 0x0123_4567_89ab_cdef);
        let mut code = first.clone();
        code.push(0x90);
        let second_start = code.len();
        code.extend_from_slice(&second);

        let mut capture = RelocationCapture::new(true);
        capture.record_x86_imm64(second_start, code.len(), 12, RelocationTarget::GcCageBase);
        capture.record_x86_imm64(0, first.len(), 3, runtime_stub());
        let rendered = capture.render(&code).unwrap();
        let document: Value = serde_json::from_str(&rendered.json).unwrap();
        assert_eq!(document["offsetBasis"], "code.bin");
        assert_eq!(document["addressEncoding"], "symbolicOnly");
        assert_eq!(document["relocations"][0]["startOffset"], 0);
        assert_eq!(
            document["relocations"][1]["startOffset"],
            second_start as u64
        );
        assert_eq!(
            document["relocations"][0]["chunks"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
        assert!(!rendered.json.contains("18369614221520256751"));
        assert!(!rendered.json.contains("81985529216486895"));
        assert_eq!(
            &rendered.normalized_code[NORMALIZED_MAGIC.len()..NORMALIZED_MAGIC.len() + 2],
            &NORMALIZED_ARCH_X86_64.to_le_bytes()
        );
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn x86_normalization_replaces_process_addresses_with_semantics() {
        let first = render_x86_single(11, 1, runtime_stub());
        let second = render_x86_single(11, u64::MAX, runtime_stub());
        assert_eq!(first.normalized_code, second.normalized_code);
        assert_ne!(first.json, "");

        let targets = [
            runtime_stub(),
            RelocationTarget::GcCageBase,
            RelocationTarget::PropertyIcSlot {
                function_id: 3,
                byte_pc: 7,
            },
            function_entry_cell(12),
        ];
        let normalized: BTreeSet<_> = targets
            .into_iter()
            .map(|target| render_x86_single(11, 0x1234, target).normalized_code)
            .collect();
        assert_eq!(normalized.len(), 4);
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn x86_rejects_malformed_ranges_registers_and_overlaps() {
        let code = x86_mov(8, 7);
        let error_for = |start, end, register| {
            let mut capture = RelocationCapture::new(true);
            capture.record_x86_imm64(start, end, register, runtime_stub());
            capture.render(&code).unwrap_err()
        };
        assert_eq!(error_for(0, 0, 8), RelocationError::EmptyRange { start: 0 });
        assert_eq!(
            error_for(0, 11, 8),
            RelocationError::RangeOutOfBounds {
                start: 0,
                end: 11,
                code_len: 10,
            }
        );
        assert_eq!(
            error_for(0, 10, 16),
            RelocationError::InvalidRegister {
                start: 0,
                register: 16,
            }
        );
        assert_eq!(
            error_for(0, 9, 8),
            RelocationError::ExpectedX86Imm64Move {
                start: 0,
                end: 9,
                register: 8,
            }
        );

        let mut two = x86_mov(3, 1);
        two.extend_from_slice(&x86_mov(4, 2));
        let mut capture = RelocationCapture::new(true);
        capture.record_x86_imm64(0, 10, 3, runtime_stub());
        capture.record_x86_imm64(9, 19, 4, RelocationTarget::GcCageBase);
        assert_eq!(
            capture.render(&two).unwrap_err(),
            RelocationError::OverlappingRanges {
                previous_start: 0,
                previous_end: 10,
                start: 9,
                end: 19,
            }
        );
    }

    #[cfg(target_arch = "x86_64")]
    #[test]
    fn x86_disabled_capture_keeps_raw_code_and_target_header() {
        let code = [0x90, 0xc3];
        let mut capture = RelocationCapture::default();
        capture.record_x86_imm64(0, 2, 0, runtime_stub());
        assert!(capture.records.is_empty());
        assert_eq!(capture.records.capacity(), 0);
        let rendered = capture.render(&code).unwrap();
        assert!(rendered.normalized_code.starts_with(NORMALIZED_MAGIC));
        assert_eq!(
            &rendered.normalized_code[NORMALIZED_MAGIC.len()..NORMALIZED_MAGIC.len() + 2],
            &NORMALIZED_ARCH_X86_64.to_le_bytes()
        );
        assert!(rendered.normalized_code.ends_with(&code));
    }

    #[cfg(target_arch = "aarch64")]
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

    #[cfg(target_arch = "aarch64")]
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

    #[cfg(target_arch = "aarch64")]
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

    #[cfg(target_arch = "aarch64")]
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
        assert_eq!(
            RelocationTarget::runtime_stub(otter_vm::native_abi::STUB_JIT_LOAD_PROPERTY),
            RelocationTarget::RuntimeStub {
                id: 17,
                name: "jit_load_property_value",
                signature: "reentrantNamedLoad",
            }
        );
        assert_eq!(
            RelocationTarget::runtime_stub(otter_vm::native_abi::STUB_JIT_STORE_PROPERTY),
            RelocationTarget::RuntimeStub {
                id: 18,
                name: "jit_store_property_value",
                signature: "reentrantNamedStore",
            }
        );
        assert_eq!(
            RelocationTarget::runtime_stub(otter_vm::native_abi::STUB_JIT_CALL_GENERIC),
            RelocationTarget::RuntimeStub {
                id: 89,
                name: "jit_call_generic",
                signature: "jsCall",
            }
        );
        assert_eq!(
            RelocationTarget::runtime_stub(otter_vm::native_abi::STUB_CREATE_CONTEXT_ALLOC),
            RelocationTarget::RuntimeStub {
                id: 25,
                name: "create_context_alloc",
                signature: "allocValue3",
            }
        );
        assert_eq!(
            RelocationTarget::runtime_stub(otter_vm::native_abi::STUB_COPY_CONTEXT_ALLOC),
            RelocationTarget::RuntimeStub {
                id: 27,
                name: "copy_context_alloc",
                signature: "allocValue3",
            }
        );
    }

    #[cfg(target_arch = "aarch64")]
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

    #[cfg(target_arch = "aarch64")]
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
            RelocationTarget::PropertyIcSlot {
                function_id: 1,
                byte_pc: 2,
            },
            RelocationTarget::CalleeIdentityCell {
                function_id: 3,
                call_pc: 4,
            },
            RelocationTarget::GuardedHeapReference {
                component: GuardedHeapComponent::Prototype,
                byte_pc: 8,
                runtime_stub_id: 9,
            },
            function_entry_cell(12),
        ];
        let normalized: BTreeSet<_> = targets
            .into_iter()
            .map(|target| render_single(&code, 4, target).normalized_code)
            .collect();
        assert_eq!(normalized.len(), 6);

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

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn function_entry_cell_identity_is_the_function_id() {
        let code = instructions(&[movz(16, 0x1234, 0, true)]);
        let first = render_single(&code, 4, function_entry_cell(12));
        let other = render_single(&code, 4, function_entry_cell(13));
        assert_ne!(first.normalized_code, other.normalized_code);
        let document: Value = serde_json::from_str(&first.json).unwrap();
        let target = &document["relocations"][0]["target"];
        assert_eq!(target["kind"], "functionEntryCell");
        assert_eq!(target["functionId"], 12);
    }

    #[cfg(target_arch = "aarch64")]
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

    #[cfg(target_arch = "aarch64")]
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

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn rejects_unsupported_pc_relative_instructions() {
        for (instruction, name) in [(0x9000_0000, "ADRP"), (0x5400_0010, "BC.cond")] {
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

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn literal_loads_address_recorded_data_words() {
        // ldr d0, #8 ; ret ; one pool word that would decode as `b`.
        let code = instructions(&[0x5c00_0040, RET, 0x1400_0000, 0]);
        let mut capture = RelocationCapture::new(true);
        capture.record_data(8, 16);
        let rendered = capture.render(&code).expect("a literal pool renders");
        let tail = &rendered.normalized_code[14..];
        assert_eq!(tail[..6], [ITEM_DIRECT_BRANCH, 8, 1, 1, 0, 2]);
        assert!(tail.ends_with(&[ITEM_DATA_WORD, 0, 0, 0, 0x14, ITEM_DATA_WORD, 0, 0, 0, 0]));
    }

    #[cfg(target_arch = "aarch64")]
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

    #[cfg(target_arch = "aarch64")]
    #[test]
    fn rejects_non_instruction_sized_code() {
        assert_eq!(
            RelocationCapture::default().render(&[0, 1, 2]).unwrap_err(),
            RelocationError::CodeLengthNotInstructionAligned { code_len: 3 }
        );
    }
}
