//! Default-off capture helpers for owned JIT artifact bundles.
//!
//! # Contents
//! - [`ArtifactRequest`] — compiler-local capture identity derived from the
//!   VM-owned diagnostics request.
//! - [`NativeCompileOutput`] — finalized code plus an optional sidecar.
//! - [`CodeMapCapture`] and [`CodeRegion`] — emission-order native offset
//!   correlation.
//! - Deterministic bytecode, safepoint, deopt, and bundle renderers.
//!
//! # Invariants
//! - Callers construct these values only when artifact capture is requested.
//!   The ordinary compile path does not allocate maps, clone executable bytes,
//!   or format text.
//! - Code regions are recorded during the existing emission pass. This module
//!   never re-emits code or runs a second lowering/analysis traversal.
//! - Exact executable bytes remain runtime-local and are never retained by the
//!   installed hot code object through this sidecar.
//! - Files are returned as owned VM DTOs; this crate performs no filesystem
//!   I/O.
//!
//! # See also
//! - [`otter_vm::jit_artifact`] for the public bundle contract.
//! - [`crate::template`] and [`crate::optimizing`] for tier-specific capture.

#![cfg_attr(not(target_arch = "aarch64"), allow(dead_code, unused_imports))]

use std::fmt::Write as _;

use otter_vm::{
    JitArtifactBundle, JitArtifactFile, JitArtifactFileName, JitArtifactIdentity,
    JitArtifactMetadata, JitCompileSnapshot, JitDebugTarget, JitDebugTier, SafepointRecord,
    TaggedLocationKind,
    deopt::{DeoptLocation, DeoptRepr, DeoptTable},
};
use serde::Serialize;

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
    logical_pc: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    byte_pc: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    operation_index: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    operation: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    deopt_exit_id: Option<u32>,
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
            logical_pc: None,
            byte_pc: None,
            operation_index: None,
            operation: None,
            deopt_exit_id: None,
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
            logical_pc: Some(logical_pc),
            byte_pc: Some(byte_pc),
            operation_index,
            operation: Some(operation),
            deopt_exit_id: None,
        }
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

    fn render(self, entry_offset: usize) -> String {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Document {
            #[serde(rename = "otterJitCodeMapSchemaVersion")]
            schema_version: u32,
            entry_offset: u64,
            regions: Vec<CodeRegion>,
            osr_entries: Vec<OsrCodeEntry>,
        }

        let document = Document {
            schema_version: 1,
            entry_offset: entry_offset as u64,
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
    deopt_table: Option<&DeoptTable>,
    safepoints: &[SafepointRecord],
) -> Option<Box<JitArtifactBundle>> {
    let mut files = vec![
        JitArtifactFile::text(JitArtifactFileName::Bytecode, render_bytecode(view)),
        JitArtifactFile::text(tier_input_name, tier_input),
        JitArtifactFile::binary(JitArtifactFileName::Code, code.bytes().to_vec()),
        JitArtifactFile::text(
            JitArtifactFileName::CodeMap,
            code_map.render(code.entry_offset()),
        ),
        JitArtifactFile::text(
            JitArtifactFileName::Safepoints,
            render_safepoints(safepoints),
        ),
    ];
    if let Some(table) = deopt_table {
        files.push(JitArtifactFile::text(
            JitArtifactFileName::Deopt,
            render_deopt(table),
        ));
    }
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
    match JitArtifactBundle::new(metadata, files) {
        Ok(bundle) => Some(Box::new(bundle)),
        Err(error) => {
            debug_assert!(false, "compiler built an invalid JIT artifact: {error}");
            None
        }
    }
}

fn render_bytecode(view: &JitCompileSnapshot) -> String {
    let mut out = String::from("; otter bytecode v1\n");
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
        #[serde(rename = "otterJitSafepointSchemaVersion")]
        schema_version: u32,
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
    let mut rendered = serde_json::to_string_pretty(&Document {
        schema_version: 1,
        safepoints,
    })
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
        #[serde(rename = "otterJitDeoptSchemaVersion")]
        schema_version: u32,
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
    let mut rendered = serde_json::to_string_pretty(&Document {
        schema_version: 1,
        exits,
    })
    .expect("deopt DTO always serializes");
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
    fn code_map_schema_is_versioned_and_offset_based() {
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
        let value: serde_json::Value =
            serde_json::from_str(&map.render(4)).expect("valid code-map JSON");
        assert_eq!(value["otterJitCodeMapSchemaVersion"], 1);
        assert_eq!(value["entryOffset"], 4);
        assert_eq!(value["regions"][0]["startOffset"], 4);
        assert_eq!(value["regions"][0]["bytePc"], 19);
        assert_eq!(value["osrEntries"][0]["logicalPc"], 2);
    }
}
