//! Default-off capture helpers for owned JIT artifact bundles.
//!
//! # Contents
//! - [`ArtifactRequest`] — compiler-local capture identity derived from the
//!   VM-owned diagnostics request.
//! - [`NativeCompileOutput`] — finalized code plus an optional sidecar.
//! - [`CodeMapCapture`] and [`CodeRegion`] — emission-order native offset
//!   correlation, including template inline subregions, generated direct-call
//!   targets, and compact scratch layouts.
//! - [`relocation`] — typed address sites and portable semantic code.
//! - [`assembly`] — deterministic annotated AArch64 disassembly.
//! - Deterministic bytecode, safepoint, deopt, and bundle renderers.
//!
//! # Invariants
//! - Callers construct these values only when artifact capture is requested.
//!   The ordinary compile path does not allocate maps, clone executable bytes,
//!   or format text.
//! - Code regions are recorded during the existing emission pass. This module
//!   never re-emits code or runs a second lowering/analysis traversal.
//! - Relocation normalization scans finalized bytes only when capture is
//!   enabled. It never changes installed code or serializes resolved
//!   relocation targets.
//! - `code-map.json` includes the opt-in capture process's executable mapping
//!   range so native sampler PCs can be joined to `codeObjectId` and emitted
//!   regions. The range is runtime-local diagnostic data, never a portable
//!   identity or executable input.
//! - A requested bundle is either returned complete or an internal compiler
//!   invariant fails loudly; release builds never silently drop diagnostics.
//! - Exact executable bytes remain runtime-local and are never retained by the
//!   installed hot code object through this sidecar.
//! - Files are returned as owned VM DTOs; this crate performs no filesystem
//!   I/O.
//!
//! # See also
//! - [`otter_vm::jit_artifact`] for the public bundle contract.
//! - [`crate::template`] and [`crate::optimizing`] for tier-specific capture.

#![cfg_attr(not(target_arch = "aarch64"), allow(dead_code, unused_imports))]

#[cfg(target_arch = "aarch64")]
mod assembly;
pub(crate) mod relocation;

use std::fmt::Write as _;

use otter_vm::{
    JitArtifactBundle, JitArtifactFile, JitArtifactFileName, JitArtifactIdentity,
    JitArtifactMetadata, JitCompileSnapshot, JitDebugTarget, JitDebugTier, SafepointRecord,
    TaggedLocationKind,
    deopt::{DeoptLocation, DeoptRepr, DeoptTable},
};
use serde::Serialize;

use self::relocation::RelocationCapture;
use crate::CompiledCode;

/// Compiler-local capture request for one successful compile.
#[derive(Debug, Clone)]
pub(crate) struct ArtifactRequest {
    pub(crate) identity: JitArtifactIdentity,
    pub(crate) tier: JitDebugTier,
    pub(crate) entry: JitDebugTarget,
}

/// Native code returned independently from its optional diagnostics sidecar.
pub(crate) struct NativeCompileOutput<T> {
    pub(crate) code: T,
    pub(crate) artifact: Option<Box<JitArtifactBundle>>,
    pub(crate) diagnostics: Box<[otter_vm::JitCompilerDiagnostic]>,
}

/// Machine-readable compact scratch assignment attached to an inline setup
/// region when artifact capture is enabled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InlineScratchLayoutArtifact {
    pub(crate) parameter_count: u16,
    pub(crate) virtual_register_count: u16,
    pub(crate) scratch_slot_count: u16,
    pub(crate) slot_bytes: u32,
    pub(crate) stack_alignment_bytes: u32,
    pub(crate) scratch_bytes: u32,
    pub(crate) offset_basis: &'static str,
    pub(crate) register_slots: Vec<Option<u16>>,
    pub(crate) receiver_slot: Option<u16>,
    pub(crate) entry_values: Vec<InlineScratchEntryArtifact>,
}

/// Caller site that owns one template-spliced plain-call or method body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct InlineSiteArtifact {
    pub(crate) caller_function_id: u32,
    pub(crate) logical_pc: u32,
    pub(crate) byte_pc: u32,
    pub(crate) has_receiver_property: bool,
}

/// Native tier of one exact generated direct-call target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum DirectCallTierArtifact {
    Template,
    Optimizing,
}

impl DirectCallTierArtifact {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Template => "template",
            Self::Optimizing => "optimizing",
        }
    }
}

/// Source opcode represented by one generated call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum DirectCallKindArtifact {
    Plain,
    Method,
}

impl DirectCallKindArtifact {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Plain => "plain",
            Self::Method => "method",
        }
    }
}

/// ECMAScript receiver source used by one generated call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum DirectCallThisModeArtifact {
    StrictOrLexical,
    SloppyGlobal,
    MethodReceiver,
}

impl DirectCallThisModeArtifact {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::StrictOrLexical => "strictOrLexical",
            Self::SloppyGlobal => "sloppyGlobal",
            Self::MethodReceiver => "methodReceiver",
        }
    }
}

/// Exact target generation and stack contract baked into one direct-call site.
///
/// `target_code_object_id` is diagnostic identity for exact artifacts. Portable
/// normalized code deliberately excludes it while retaining the semantic
/// receiver mode, tier, and every layout field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct DirectCallArtifact {
    pub(crate) call_kind: DirectCallKindArtifact,
    pub(crate) target_function_id: u32,
    pub(crate) target_code_object_id: u64,
    pub(crate) target_tier: DirectCallTierArtifact,
    pub(crate) this_mode: DirectCallThisModeArtifact,
    pub(crate) callee_native_frame_bytes: u32,
    pub(crate) linkage_bytes: u32,
    pub(crate) reserved_stack_bytes: u32,
    pub(crate) callee_register_count: u16,
}

/// Exact heap facts re-read by one guarded monomorphic method edge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MethodGuardArtifact {
    pub(crate) receiver_register: u16,
    pub(crate) method_function_id: u32,
    pub(crate) receiver_shape: u32,
    pub(crate) prototype_shapes: Vec<u32>,
    pub(crate) method_value_byte: u32,
}

/// One live-in value materialized by an inline scratch setup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub(crate) enum InlineScratchEntryArtifact {
    Argument {
        argument: u16,
        register: u16,
        slot: u16,
    },
    Receiver {
        slot: u16,
    },
    Undefined {
        register: u16,
        slot: u16,
    },
}

/// One emitted native region.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CodeRegion {
    kind: &'static str,
    start_offset: u64,
    end_offset: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    block: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_block: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    inline_frame: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    function_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    direct_call: Option<DirectCallArtifact>,
    #[serde(skip_serializing_if = "Option::is_none")]
    method_guard: Option<MethodGuardArtifact>,
    #[serde(skip_serializing_if = "Option::is_none")]
    static_native_call: Option<otter_vm::JitStaticNativeCallKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    logical_pc: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    byte_pc: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    operation_index: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    operation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    deopt_exit_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    inline_scratch_layout: Option<InlineScratchLayoutArtifact>,
    #[serde(skip_serializing_if = "Option::is_none")]
    inline_site: Option<InlineSiteArtifact>,
}

impl CodeRegion {
    pub(crate) fn structural(kind: &'static str, start: usize, end: usize) -> Self {
        Self {
            kind,
            start_offset: start as u64,
            end_offset: end as u64,
            block: None,
            target_block: None,
            inline_frame: None,
            function_id: None,
            direct_call: None,
            method_guard: None,
            static_native_call: None,
            logical_pc: None,
            byte_pc: None,
            operation_index: None,
            operation: None,
            deopt_exit_id: None,
            inline_scratch_layout: None,
            inline_site: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn instruction(
        start: usize,
        end: usize,
        block: Option<u32>,
        inline_frame: Option<u32>,
        function_id: u32,
        logical_pc: u32,
        byte_pc: u32,
        operation_index: Option<u32>,
        operation: String,
    ) -> Self {
        Self {
            kind: "instruction",
            start_offset: start as u64,
            end_offset: end as u64,
            block,
            target_block: None,
            inline_frame,
            function_id: Some(function_id),
            direct_call: None,
            method_guard: None,
            static_native_call: None,
            logical_pc: Some(logical_pc),
            byte_pc: Some(byte_pc),
            operation_index,
            operation: Some(operation),
            deopt_exit_id: None,
            inline_scratch_layout: None,
            inline_site: None,
        }
    }

    pub(crate) fn inline_structural(
        kind: &'static str,
        start: usize,
        end: usize,
        inline_site: InlineSiteArtifact,
        function_id: u32,
    ) -> Self {
        let mut region = Self::structural(kind, start, end);
        region.function_id = Some(function_id);
        region.inline_site = Some(inline_site);
        region
    }

    /// One compiler-generated call phase.
    ///
    /// `function_id` remains the caller owning this code object. `direct_call`
    /// carries the exact target generation and stack contract separately.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn call_structural(
        kind: &'static str,
        start: usize,
        end: usize,
        caller_function_id: u32,
        logical_pc: u32,
        byte_pc: u32,
        direct_call: DirectCallArtifact,
    ) -> Self {
        let mut region = Self::structural(kind, start, end);
        region.function_id = Some(caller_function_id);
        region.direct_call = Some(direct_call);
        region.logical_pc = Some(logical_pc);
        region.byte_pc = Some(byte_pc);
        region
    }

    /// Heap identity guard immediately preceding a generated method call.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn method_call_structural(
        kind: &'static str,
        start: usize,
        end: usize,
        caller_function_id: u32,
        logical_pc: u32,
        byte_pc: u32,
        direct_call: DirectCallArtifact,
        receiver_register: u16,
        guard: &otter_vm::jit::JitMethodGuard,
    ) -> Self {
        let mut region = Self::call_structural(
            kind,
            start,
            end,
            caller_function_id,
            logical_pc,
            byte_pc,
            direct_call,
        );
        region.method_guard = Some(MethodGuardArtifact {
            receiver_register,
            method_function_id: guard.method_fid,
            receiver_shape: guard.recv_shape,
            prototype_shapes: guard.proto_chain.clone(),
            method_value_byte: guard.method_value_byte,
        });
        region
    }

    /// One guarded static-native ordinary-call phase.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn static_native_structural(
        kind: &'static str,
        start: usize,
        end: usize,
        caller_function_id: u32,
        logical_pc: u32,
        byte_pc: u32,
        target: otter_vm::JitStaticNativeCallKind,
    ) -> Self {
        let mut region = Self::structural(kind, start, end);
        region.function_id = Some(caller_function_id);
        region.static_native_call = Some(target);
        region.logical_pc = Some(logical_pc);
        region.byte_pc = Some(byte_pc);
        region
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn inline_instruction(
        start: usize,
        end: usize,
        inline_site: InlineSiteArtifact,
        function_id: u32,
        logical_pc: u32,
        byte_pc: u32,
        operation_index: u32,
        operation: String,
    ) -> Self {
        let mut region =
            Self::inline_structural("inlineInstruction", start, end, inline_site, function_id);
        region.logical_pc = Some(logical_pc);
        region.byte_pc = Some(byte_pc);
        region.operation_index = Some(operation_index);
        region.operation = Some(operation);
        region
    }

    pub(crate) fn inline_scratch(
        start: usize,
        end: usize,
        inline_site: InlineSiteArtifact,
        function_id: u32,
        layout: InlineScratchLayoutArtifact,
    ) -> Self {
        let mut region =
            Self::inline_structural("inlineScratchSetup", start, end, inline_site, function_id);
        region.inline_scratch_layout = Some(layout);
        region
    }

    pub(crate) fn deopt(start: usize, end: usize, exit_id: u32, logical_pc: u32) -> Self {
        let mut region = Self::structural("deoptExit", start, end);
        region.logical_pc = Some(logical_pc);
        region.deopt_exit_id = Some(exit_id);
        region
    }

    pub(crate) fn block(kind: &'static str, start: usize, end: usize, block: u32) -> Self {
        let mut region = Self::structural(kind, start, end);
        region.block = Some(block);
        region
    }

    pub(crate) fn edge(
        kind: &'static str,
        start: usize,
        end: usize,
        block: u32,
        target_block: u32,
    ) -> Self {
        let mut region = Self::block(kind, start, end, block);
        region.target_block = Some(target_block);
        region
    }
}

/// One loop-header native entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct OsrCodeEntry {
    logical_pc: u32,
    start_offset: u64,
    end_offset: u64,
}

/// Optional emission-side code-map storage.
#[derive(Debug, Default)]
pub(crate) struct CodeMapCapture {
    regions: Vec<CodeRegion>,
    osr_entries: Vec<OsrCodeEntry>,
}

impl CodeMapCapture {
    pub(crate) fn record(&mut self, region: CodeRegion) {
        self.regions.push(region);
    }

    pub(crate) fn record_osr(&mut self, logical_pc: u32, start: usize, end: usize) {
        self.osr_entries.push(OsrCodeEntry {
            logical_pc,
            start_offset: start as u64,
            end_offset: end as u64,
        });
    }

    fn render(self, entry_offset: usize, runtime_range: Option<(usize, usize)>) -> String {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct RuntimeAddressRange {
            start: String,
            end_exclusive: String,
            entry: String,
        }

        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Document {
            entry_offset: u64,
            #[serde(skip_serializing_if = "Option::is_none")]
            runtime_address_range: Option<RuntimeAddressRange>,
            regions: Vec<CodeRegion>,
            osr_entries: Vec<OsrCodeEntry>,
        }

        let runtime_address_range =
            runtime_range.map(|(start, end_exclusive)| RuntimeAddressRange {
                start: format!("0x{start:x}"),
                end_exclusive: format!("0x{end_exclusive:x}"),
                entry: format!("0x{:x}", start.saturating_add(entry_offset)),
            });
        let document = Document {
            entry_offset: entry_offset as u64,
            runtime_address_range,
            regions: self.regions,
            osr_entries: self.osr_entries,
        };
        let mut rendered =
            serde_json::to_string_pretty(&document).expect("code-map DTO always serializes");
        rendered.push('\n');
        rendered
    }
}

/// Join tier input and emission products into the VM-owned bundle.
#[allow(clippy::too_many_arguments)]
pub(crate) fn build_bundle(
    request: ArtifactRequest,
    view: &JitCompileSnapshot,
    code_object_id: u64,
    code: &CompiledCode,
    tier_input_name: JitArtifactFileName,
    tier_input: String,
    code_map: CodeMapCapture,
    relocations: RelocationCapture,
    deopt_table: Option<&DeoptTable>,
    safepoints: &[SafepointRecord],
) -> Box<JitArtifactBundle> {
    let rendered_relocations = relocations
        .render(code.bytes())
        .unwrap_or_else(|error| panic!("compiler built invalid JIT relocations: {error}"));
    let metadata = JitArtifactMetadata {
        target: env!("OTTER_JIT_TARGET").to_string(),
        architecture: std::env::consts::ARCH.to_string(),
        operating_system: std::env::consts::OS.to_string(),
        tier: request.tier,
        function_id: view.code_block.id,
        function_name: request.identity.function_name,
        module: request.identity.module,
        code_object_id,
        entry: request.entry,
        bytecode_bytes: u64::from(view.code_block.bytecode_byte_len()),
        code_bytes: u64::try_from(code.len()).unwrap_or(u64::MAX),
    };
    #[cfg(target_arch = "aarch64")]
    let rendered_assembly = assembly::render(
        &metadata,
        code.bytes(),
        code.entry_offset(),
        &code_map,
        &rendered_relocations.validated,
        deopt_table,
        safepoints,
    );
    let mut files = vec![
        JitArtifactFile::text(JitArtifactFileName::Bytecode, render_bytecode(view)),
        JitArtifactFile::text(tier_input_name, tier_input),
        JitArtifactFile::binary(JitArtifactFileName::Code, code.bytes().to_vec()),
        JitArtifactFile::binary(
            JitArtifactFileName::NormalizedCode,
            rendered_relocations.normalized_code,
        ),
        JitArtifactFile::text(
            JitArtifactFileName::CodeMap,
            code_map.render(
                code.entry_offset(),
                Some((
                    code.bytes().as_ptr() as usize,
                    (code.bytes().as_ptr() as usize).saturating_add(code.len()),
                )),
            ),
        ),
        JitArtifactFile::text(JitArtifactFileName::Relocations, rendered_relocations.json),
        JitArtifactFile::text(
            JitArtifactFileName::Safepoints,
            render_safepoints(safepoints),
        ),
    ];
    #[cfg(target_arch = "aarch64")]
    files.push(JitArtifactFile::text(
        JitArtifactFileName::Assembly,
        rendered_assembly,
    ));
    if let Some(table) = deopt_table {
        files.push(JitArtifactFile::text(
            JitArtifactFileName::Deopt,
            render_deopt(table),
        ));
    }
    Box::new(
        JitArtifactBundle::new(metadata, files)
            .unwrap_or_else(|error| panic!("compiler built an invalid JIT artifact: {error}")),
    )
}

fn render_bytecode(view: &JitCompileSnapshot) -> String {
    let mut out = String::from("; otter bytecode\n");
    writeln!(
        out,
        "; function={} registers={} parameters={} bytes={}",
        view.code_block.id,
        view.code_block.register_count,
        view.code_block.param_count,
        view.code_block.bytecode_byte_len()
    )
    .expect("writing to String cannot fail");
    for instruction in &view.instructions {
        writeln!(
            out,
            "{:04} byte={:04} {:?} {:?}",
            instruction.instruction_pc(view.code_block.as_ref()),
            instruction.byte_pc(),
            instruction.op(view.code_block.as_ref()),
            instruction.operand_view(view.code_block.as_ref())
        )
        .expect("writing to String cannot fail");
    }
    out
}

fn render_safepoints(records: &[SafepointRecord]) -> String {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Location {
        kind: &'static str,
        index: u16,
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Point {
        id: u32,
        frame_state: u32,
        native_return_offset: Option<u64>,
        tagged_locations: Vec<Location>,
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Document {
        safepoints: Vec<Point>,
    }

    let safepoints = records
        .iter()
        .map(|record| Point {
            id: record.id,
            frame_state: record.frame_state,
            // Native correlation is added by the relocation/safepoint-site
            // follow-up. Null is explicit and never fabricates an offset.
            native_return_offset: None,
            tagged_locations: record
                .tagged_locations
                .iter()
                .map(|location| Location {
                    kind: match location.kind {
                        TaggedLocationKind::FrameSlot => "frameSlot",
                        TaggedLocationKind::MachineRegister => "machineRegister",
                        TaggedLocationKind::SpillSlot => "spillSlot",
                    },
                    index: location.index,
                })
                .collect(),
        })
        .collect();
    let mut rendered = serde_json::to_string_pretty(&Document { safepoints })
        .expect("safepoint DTO always serializes");
    rendered.push('\n');
    rendered
}

fn render_deopt(table: &DeoptTable) -> String {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Slot {
        location_kind: &'static str,
        location_value: i64,
        representation: &'static str,
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Frame {
        function_id: u32,
        byte_pc: u32,
        slots: Vec<Slot>,
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Exit {
        id: u32,
        frames: Vec<Frame>,
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Document {
        exits: Vec<Exit>,
    }

    let exits = table
        .entries()
        .iter()
        .enumerate()
        .map(|(id, state)| Exit {
            id: u32::try_from(id).unwrap_or(u32::MAX),
            frames: state
                .frames
                .iter()
                .map(|frame| Frame {
                    function_id: frame.function_id,
                    byte_pc: frame.byte_pc,
                    slots: frame
                        .slots
                        .iter()
                        .map(|slot| {
                            let (location_kind, location_value) = match slot.location {
                                DeoptLocation::Register(register) => {
                                    ("register", i64::from(register))
                                }
                                DeoptLocation::StackSlot(offset) => {
                                    ("stackSlot", i64::from(offset))
                                }
                                DeoptLocation::Constant(index) => ("constant", i64::from(index)),
                            };
                            Slot {
                                location_kind,
                                location_value,
                                representation: match slot.repr {
                                    DeoptRepr::Tagged => "tagged",
                                    DeoptRepr::Int32 => "int32",
                                    DeoptRepr::Float64 => "float64",
                                },
                            }
                        })
                        .collect(),
                })
                .collect(),
        })
        .collect();
    let mut rendered =
        serde_json::to_string_pretty(&Document { exits }).expect("deopt DTO always serializes");
    rendered.push('\n');
    rendered
}

#[cfg(test)]
mod tests {
    use otter_bytecode::{Op, Operand};
    use otter_vm::jit::JitTestInstruction;

    use super::*;

    #[test]
    fn bytecode_render_is_deterministic_and_uses_both_pc_domains() {
        let view = JitCompileSnapshot::without_feedback(
            9,
            0,
            1,
            vec![JitTestInstruction::new(
                Op::ReturnUndefined,
                0,
                17,
                Vec::<Operand>::new(),
            )],
        );
        let first = render_bytecode(&view);
        assert_eq!(first, render_bytecode(&view));
        assert!(first.contains("0000 byte=0017 ReturnUndefined []"));
    }

    #[test]
    fn code_map_format_is_typed_and_offset_based() {
        let mut map = CodeMapCapture::default();
        map.record(CodeRegion::instruction(
            4,
            12,
            None,
            None,
            7,
            2,
            19,
            Some(0),
            "Move { dst: 1, src: 0 }".to_string(),
        ));
        map.record_osr(2, 20, 32);
        let value: serde_json::Value = serde_json::from_str(&map.render(4, Some((0x1000, 0x1040))))
            .expect("valid code-map JSON");
        assert_eq!(value["entryOffset"], 4);
        assert_eq!(value["runtimeAddressRange"]["start"], "0x1000");
        assert_eq!(value["runtimeAddressRange"]["endExclusive"], "0x1040");
        assert_eq!(value["runtimeAddressRange"]["entry"], "0x1004");
        assert_eq!(value["regions"][0]["startOffset"], 4);
        assert_eq!(value["regions"][0]["bytePc"], 19);
        assert_eq!(value["osrEntries"][0]["logicalPc"], 2);
    }

    #[test]
    fn code_map_direct_call_shape_names_exact_generation_and_stack_contract() {
        let mut map = CodeMapCapture::default();
        map.record(CodeRegion::call_structural(
            "directCallNativeEntry",
            20,
            28,
            7,
            2,
            19,
            DirectCallArtifact {
                call_kind: DirectCallKindArtifact::Plain,
                target_function_id: 11,
                target_code_object_id: 29,
                target_tier: DirectCallTierArtifact::Optimizing,
                this_mode: DirectCallThisModeArtifact::SloppyGlobal,
                callee_native_frame_bytes: 160,
                linkage_bytes: 112,
                reserved_stack_bytes: 272,
                callee_register_count: 6,
            },
        ));

        let value: serde_json::Value =
            serde_json::from_str(&map.render(0, None)).expect("valid code-map JSON");
        let region = &value["regions"][0];
        assert_eq!(region["functionId"], 7);
        assert_eq!(region["logicalPc"], 2);
        assert_eq!(region["bytePc"], 19);
        assert_eq!(region["directCall"]["callKind"], "plain");
        assert_eq!(region["directCall"]["targetFunctionId"], 11);
        assert_eq!(region["directCall"]["targetCodeObjectId"], 29);
        assert_eq!(region["directCall"]["targetTier"], "optimizing");
        assert_eq!(region["directCall"]["thisMode"], "sloppyGlobal");
        assert_eq!(region["directCall"]["calleeNativeFrameBytes"], 160);
        assert_eq!(region["directCall"]["linkageBytes"], 112);
        assert_eq!(region["directCall"]["reservedStackBytes"], 272);
        assert_eq!(region["directCall"]["calleeRegisterCount"], 6);
    }

    #[test]
    fn code_map_inline_scratch_layout_is_typed_and_offset_safe() {
        let inline_site = InlineSiteArtifact {
            caller_function_id: 7,
            logical_pc: 2,
            byte_pc: 19,
            has_receiver_property: true,
        };
        let layout = InlineScratchLayoutArtifact {
            parameter_count: 1,
            virtual_register_count: 3,
            scratch_slot_count: 3,
            slot_bytes: 8,
            stack_alignment_bytes: 16,
            scratch_bytes: 32,
            offset_basis: "postAllocationSp",
            register_slots: vec![Some(0), None, Some(1)],
            receiver_slot: Some(2),
            entry_values: vec![
                InlineScratchEntryArtifact::Argument {
                    argument: 0,
                    register: 0,
                    slot: 0,
                },
                InlineScratchEntryArtifact::Receiver { slot: 2 },
                InlineScratchEntryArtifact::Undefined {
                    register: 2,
                    slot: 1,
                },
            ],
        };
        let mut map = CodeMapCapture::default();
        map.record(CodeRegion::structural("entry", 0, 4));
        map.record(CodeRegion::inline_scratch(4, 20, inline_site, 11, layout));

        let value: serde_json::Value =
            serde_json::from_str(&map.render(0, None)).expect("valid code-map JSON");
        let ordinary = &value["regions"][0];
        assert!(ordinary.get("inlineSite").is_none());
        assert!(ordinary.get("inlineScratchLayout").is_none());

        let inline = &value["regions"][1];
        assert_eq!(inline["kind"], "inlineScratchSetup");
        assert_eq!(inline["functionId"], 11);
        assert_eq!(inline["inlineSite"]["callerFunctionId"], 7);
        assert_eq!(inline["inlineSite"]["logicalPc"], 2);
        assert_eq!(inline["inlineSite"]["bytePc"], 19);
        assert_eq!(inline["inlineSite"]["hasReceiverProperty"], true);

        let layout = &inline["inlineScratchLayout"];
        assert_eq!(layout["parameterCount"], 1);
        assert_eq!(layout["virtualRegisterCount"], 3);
        assert_eq!(layout["scratchSlotCount"], 3);
        assert_eq!(layout["slotBytes"], 8);
        assert_eq!(layout["stackAlignmentBytes"], 16);
        assert_eq!(layout["scratchBytes"], 32);
        assert_eq!(layout["offsetBasis"], "postAllocationSp");
        assert_eq!(layout["registerSlots"][0], 0);
        assert!(layout["registerSlots"][1].is_null());
        assert_eq!(layout["receiverSlot"], 2);

        let entries = layout["entryValues"]
            .as_array()
            .expect("typed entry values");
        assert_eq!(entries[0]["kind"], "argument");
        assert_eq!(entries[0]["argument"], 0);
        assert_eq!(entries[0]["register"], 0);
        assert_eq!(entries[0]["slot"], 0);
        assert_eq!(entries[1]["kind"], "receiver");
        assert_eq!(entries[1]["slot"], 2);
        assert!(entries[1].get("register").is_none());
        assert_eq!(entries[2]["kind"], "undefined");
        assert_eq!(entries[2]["register"], 2);
        assert_eq!(entries[2]["slot"], 1);
        assert!(entries[2].get("argument").is_none());
    }
}
