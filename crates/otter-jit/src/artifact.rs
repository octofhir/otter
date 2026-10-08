//! Default-off capture helpers for owned JIT artifact bundles.
//!
//! # Contents
//! - [`ArtifactRequest`] — compiler-local capture identity derived from the
//!   VM-owned diagnostics request.
//! - [`NativeCompileOutput`] — finalized code plus an optional sidecar.
//! - [`CodeMapCapture`] and [`CodeRegion`] — emission-order native offset
//!   correlation, including template inline subregions, generated direct-call
//!   targets, compact scratch layouts and actual callable-entry offsets.
//! - [`relocation`] — typed address sites and portable semantic code.
//! - [`assembly`] — deterministic annotated target disassembly.
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

#[cfg(target_arch = "aarch64")]
mod assembly;
#[cfg(target_arch = "x86_64")]
#[path = "artifact/assembly_x86_64.rs"]
mod assembly;
pub(crate) mod relocation;
mod return_sites;

use std::fmt::Write as _;

use otter_vm::{
    JitArtifactBundle, JitArtifactFile, JitArtifactFileName, JitArtifactIdentity,
    JitArtifactMetadata, JitCompileSnapshot, JitDebugTarget, JitDebugTier,
    deopt::{DeoptLocation, DeoptRepr, DeoptRuntime},
    native_abi::{SafepointEntry, SafepointRecord},
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
    pub(crate) ir_node_count: u64,
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

/// Exact heap facts re-read by one guarded monomorphic method edge.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct MethodGuardArtifact {
    pub(crate) receiver_register: u16,
    pub(crate) method_function_id: u32,
    pub(crate) receiver_shape: u32,
    pub(crate) prototype_validity: Option<u64>,
    pub(crate) holder_root: u32,
    pub(crate) method_field: otter_vm::object::FieldLocation,
}

/// One live-in value materialized by an inline scratch setup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
#[cfg_attr(target_arch = "x86_64", allow(dead_code))]
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
    /// Proven call target entered through its current generation.
    #[serde(skip_serializing_if = "Option::is_none")]
    call_target_function_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    method_guard: Option<MethodGuardArtifact>,
    #[serde(skip_serializing_if = "Option::is_none")]
    native_leaf_call: Option<&'static str>,
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
            call_target_function_id: None,
            method_guard: None,
            native_leaf_call: None,
            logical_pc: None,
            byte_pc: None,
            operation_index: None,
            operation: None,
            deopt_exit_id: None,
            inline_scratch_layout: None,
            inline_site: None,
        }
    }

    #[cfg_attr(not(target_arch = "aarch64"), allow(dead_code))]
    pub(crate) fn structural_at_byte_pc(
        kind: &'static str,
        start: usize,
        end: usize,
        byte_pc: u32,
    ) -> Self {
        let mut region = Self::structural(kind, start, end);
        region.byte_pc = Some(byte_pc);
        region
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
            call_target_function_id: None,
            method_guard: None,
            native_leaf_call: None,
            logical_pc: Some(logical_pc),
            byte_pc: Some(byte_pc),
            operation_index,
            operation: Some(operation),
            deopt_exit_id: None,
            inline_scratch_layout: None,
            inline_site: None,
        }
    }

    #[cfg(any(test, target_arch = "aarch64"))]
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
    /// `function_id` remains the caller owning this code object;
    /// `call_target_function_id` names a proven target entered through its
    /// current generation.
    #[allow(clippy::too_many_arguments)]
    #[cfg(any(test, target_arch = "aarch64", target_arch = "x86_64"))]
    pub(crate) fn call_structural(
        kind: &'static str,
        start: usize,
        end: usize,
        caller_function_id: u32,
        logical_pc: u32,
        byte_pc: u32,
        call_target_function_id: Option<u32>,
    ) -> Self {
        let mut region = Self::structural(kind, start, end);
        region.function_id = Some(caller_function_id);
        region.call_target_function_id = call_target_function_id;
        region.logical_pc = Some(logical_pc);
        region.byte_pc = Some(byte_pc);
        region
    }

    /// Heap identity guard immediately preceding a generated method call.
    #[allow(clippy::too_many_arguments)]
    #[cfg(any(test, target_arch = "aarch64"))]
    #[cfg_attr(target_arch = "x86_64", allow(dead_code))]
    pub(crate) fn method_call_structural(
        kind: &'static str,
        start: usize,
        end: usize,
        caller_function_id: u32,
        logical_pc: u32,
        byte_pc: u32,
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
            None,
        );
        region.method_guard = Some(MethodGuardArtifact {
            receiver_register,
            method_function_id: guard.method_fid,
            receiver_shape: guard.recv_shape,
            prototype_validity: guard.prototype_validity.map(|cell| cell.identity),
            holder_root: guard.holder_root,
            method_field: guard.method_field,
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
        target: &'static str,
    ) -> Self {
        let mut region = Self::structural(kind, start, end);
        region.function_id = Some(caller_function_id);
        region.native_leaf_call = Some(target);
        region.logical_pc = Some(logical_pc);
        region.byte_pc = Some(byte_pc);
        region
    }

    #[allow(clippy::too_many_arguments)]
    #[cfg(any(test, target_arch = "aarch64"))]
    #[cfg_attr(target_arch = "x86_64", allow(dead_code))]
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

    #[cfg(any(test, target_arch = "aarch64"))]
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
    call_entry_offset: Option<usize>,
}

impl CodeMapCapture {
    /// Record the actual offset returned by the canonical frame emitter once.
    /// This is emission capability, independent of the request's compile trigger.
    pub(crate) fn record_call_entry(&mut self, offset: usize) {
        assert!(self.call_entry_offset.replace(offset).is_none());
    }

    fn validate_call_entry(&self, code_bytes: usize) {
        assert!(
            self.call_entry_offset
                .is_none_or(|offset| offset < code_bytes),
            "call entry must name an instruction inside the finalized code mapping"
        );
    }

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
            call_entry_offset: Option<u64>,
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
            call_entry_offset: self.call_entry_offset.map(|offset| {
                u64::try_from(offset).expect("native mapping offsets fit the artifact integer")
            }),
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
    deopt_runtime: Option<&DeoptRuntime>,
    safepoints: &[SafepointRecord],
    return_sites: &[SafepointEntry],
) -> Box<JitArtifactBundle> {
    code_map.validate_call_entry(code.len());
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
    let rendered_assembly = assembly::render(
        &metadata,
        code.bytes(),
        code.entry_offset(),
        &code_map,
        &rendered_relocations.validated,
        deopt_runtime.map(|runtime| &runtime.table),
        safepoints,
        return_sites,
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
            render_safepoints(safepoints, return_sites),
        ),
    ];
    files.push(JitArtifactFile::text(
        JitArtifactFileName::Assembly,
        rendered_assembly,
    ));
    if let Some(runtime) = deopt_runtime {
        files.push(JitArtifactFile::text(
            JitArtifactFileName::Deopt,
            render_deopt(runtime),
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

fn render_safepoints(records: &[SafepointRecord], return_sites: &[SafepointEntry]) -> String {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Location {
        kind: &'static str,
        index: u16,
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Point<'a> {
        id: u32,
        frame_state: u32,
        tagged_locations: Vec<Location>,
        inline_frames: &'a [otter_vm::deopt::DeoptFrame<Option<u16>>],
        call_pc: Option<u32>,
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct ReturnSite {
        native_return_offset: u32,
        safepoint_id: u32,
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Document<'a> {
        records: Vec<Point<'a>>,
        return_sites: Vec<ReturnSite>,
    }

    let safepoints = records
        .iter()
        .map(|record| Point {
            inline_frames: &record.inline_frames,
            call_pc: (record.call_pc != otter_vm::native_abi::NO_CALL_PC).then_some(record.call_pc),
            id: record.id,
            frame_state: record.frame_state,
            tagged_locations: record
                .spill_roots
                .iter()
                .map(|index| Location {
                    kind: "spillSlot",
                    index,
                })
                .collect(),
        })
        .collect();
    let mut rendered = serde_json::to_string_pretty(&Document {
        records: safepoints,
        return_sites: return_sites
            .iter()
            .map(|site| ReturnSite {
                native_return_offset: site.native_return_offset,
                safepoint_id: site.safepoint_id,
            })
            .collect(),
    })
    .expect("safepoint DTO always serializes");
    rendered.push('\n');
    rendered
}

fn render_deopt(runtime: &DeoptRuntime) -> String {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Slot {
        location_kind: &'static str,
        location_value: String,
        representation: &'static str,
    }

    fn render_slot(slot: &otter_vm::deopt::DeoptSlot) -> Slot {
        match slot.location {
            DeoptLocation::VirtualObject(object) => Slot {
                location_kind: "virtualObject",
                location_value: object.0.to_string(),
                representation: "tagged",
            },
            location => {
                let (location_kind, location_value) = match location {
                    DeoptLocation::StackSlot(offset) => ("stackSlot", offset.to_string()),
                    DeoptLocation::Literal(raw) => ("literal", format!("0x{raw:016x}")),
                    DeoptLocation::Register(index) => ("register", index.to_string()),
                    DeoptLocation::VirtualObject(_) => unreachable!(),
                };
                Slot {
                    location_kind,
                    location_value,
                    representation: match slot.repr {
                        DeoptRepr::Tagged => "tagged",
                        DeoptRepr::Int32 => "int32",
                        DeoptRepr::Boolean => "boolean",
                        DeoptRepr::Uint32 => "uint32",
                        DeoptRepr::Float64 => "float64",
                    },
                }
            }
        }
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct State {
        id: u32,
        frames: Vec<otter_vm::deopt::DeoptFrame<Slot>>,
        virtual_objects: Vec<otter_vm::deopt::VirtualObject<Slot>>,
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Exit {
        id: u32,
        frame_state_id: u32,
        reason: otter_vm::native_abi::ExitReason,
        action: otter_vm::native_abi::ExitAction,
        resume_pcs: Vec<u32>,
    }

    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Document {
        frame_states: Vec<State>,
        exits: Vec<Exit>,
    }

    let frame_states = runtime
        .table
        .indexed_entries()
        .map(|(id, state)| State {
            id,
            frames: state
                .frames
                .iter()
                .map(|frame| otter_vm::deopt::DeoptFrame {
                    function_id: frame.function_id,
                    byte_pc: frame.byte_pc,
                    entry: frame
                        .entry
                        .as_ref()
                        .map(|entry| otter_vm::deopt::DeoptFrameEntry {
                            return_register: entry.return_register,
                            this: render_slot(&entry.this),
                            closure: render_slot(&entry.closure),
                            new_target: render_slot(&entry.new_target),
                        }),
                    register_count: frame.register_count,
                    slots: frame
                        .slots
                        .iter()
                        .map(|(register, slot)| (*register, render_slot(slot)))
                        .collect(),
                })
                .collect(),
            virtual_objects: state
                .virtual_objects
                .iter()
                .map(|object| otter_vm::deopt::VirtualObject {
                    id: object.id,
                    kind: object.kind,
                    fields: object.fields.iter().map(render_slot).collect(),
                })
                .collect(),
        })
        .collect();
    let exits = runtime
        .exits
        .iter()
        .enumerate()
        .map(|(id, exit)| Exit {
            id: u32::try_from(id).unwrap_or(u32::MAX),
            frame_state_id: exit.state,
            reason: exit.reason,
            action: exit.action,
            resume_pcs: exit.resume_pcs.to_vec(),
        })
        .collect();
    let mut rendered = serde_json::to_string_pretty(&Document {
        frame_states,
        exits,
    })
    .expect("deopt DTO always serializes");
    rendered.push('\n');
    rendered
}

#[cfg(test)]
mod tests {
    #[test]
    fn deopt_artifact_preserves_inline_entry_recipes() {
        use otter_vm::deopt::{
            DeoptExitDescriptor, DeoptFrame, DeoptFrameEntry, DeoptLocation, DeoptRepr,
            DeoptRuntime, DeoptSlot, DeoptTable, FrameState, VirtualObject, VirtualObjectId,
            VirtualObjectKind,
        };
        let slot = DeoptSlot {
            location: DeoptLocation::StackSlot(24),
            repr: DeoptRepr::Tagged,
        };
        let table = DeoptTable::from_states(vec![FrameState {
            frames: Box::new([
                DeoptFrame::with_window(1, 16, None, [slot]),
                DeoptFrame::with_window(
                    2,
                    8,
                    Some(DeoptFrameEntry {
                        new_target: DeoptSlot {
                            location: DeoptLocation::Literal(
                                otter_vm::Value::function(99).to_bits(),
                            ),
                            repr: DeoptRepr::Tagged,
                        },
                        return_register: 0,
                        this: slot,
                        closure: DeoptSlot {
                            location: DeoptLocation::StackSlot(32),
                            repr: DeoptRepr::Tagged,
                        },
                    }),
                    [DeoptSlot::virtual_object(VirtualObjectId(0))],
                ),
            ]),
            virtual_objects: Box::new([VirtualObject {
                id: VirtualObjectId(0),
                kind: VirtualObjectKind::FixedArray,
                fields: Box::new([slot]),
            }]),
        }]);
        let runtime = DeoptRuntime {
            table,
            exits: vec![DeoptExitDescriptor {
                state: 0,
                reason: otter_vm::native_abi::ExitReason::ShapeGuard,
                action: otter_vm::native_abi::ExitAction::Recompile,
                resume_pcs: vec![16, 8].into_boxed_slice(),
                safepoint: otter_vm::native_abi::NO_SAFEPOINT,
            }]
            .into_boxed_slice(),
        };
        let json: serde_json::Value = serde_json::from_str(&super::render_deopt(&runtime)).unwrap();
        let frames = &json["frameStates"][0]["frames"];
        assert!(frames[0]["entry"].is_null());
        assert_eq!(frames[1]["entry"]["returnRegister"], 0);
        assert_eq!(frames[1]["entry"]["this"]["locationKind"], "stackSlot");
        assert_eq!(frames[1]["entry"]["this"]["locationValue"], "24");
        assert_eq!(frames[1]["entry"]["closure"]["locationKind"], "stackSlot");
        assert_eq!(frames[1]["entry"]["closure"]["locationValue"], "32");
        assert_eq!(frames[1]["entry"]["newTarget"]["locationKind"], "literal");
        // Sparse frames render `[register, slot]` pairs.
        assert_eq!(frames[1]["slots"][0][0], 0);
        assert_eq!(frames[1]["slots"][0][1]["locationKind"], "virtualObject");
        assert_eq!(
            json["frameStates"][0]["virtualObjects"][0]["kind"],
            "fixedArray"
        );
        assert_eq!(
            frames[1]["entry"]["newTarget"]["locationValue"],
            format!("0x{:016x}", otter_vm::Value::function(99).to_bits())
        );
        assert_eq!(json["exits"][0]["frameStateId"], 0);
        assert_eq!(json["exits"][0]["reason"], "shapeGuard");
        assert_eq!(json["exits"][0]["action"], "recompile");
        assert_eq!(json["exits"][0]["resumePcs"], serde_json::json!([16, 8]));
    }

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
        map.record_call_entry(8);
        map.validate_call_entry(64);
        let value: serde_json::Value = serde_json::from_str(&map.render(4, Some((0x1000, 0x1040))))
            .expect("valid code-map JSON");
        assert_eq!(value["entryOffset"], 4);
        assert_eq!(value["callEntryOffset"], 8);
        assert_eq!(value["runtimeAddressRange"]["start"], "0x1000");
        assert_eq!(value["runtimeAddressRange"]["endExclusive"], "0x1040");
        assert_eq!(value["runtimeAddressRange"]["entry"], "0x1004");
        assert_eq!(value["regions"][0]["startOffset"], 4);
        assert_eq!(value["regions"][0]["bytePc"], 19);
        assert_eq!(value["osrEntries"][0]["logicalPc"], 2);
    }

    #[test]
    fn code_map_without_call_entry_has_explicit_null_capability() {
        let map = CodeMapCapture::default();
        map.validate_call_entry(64);
        let value: serde_json::Value = serde_json::from_str(&map.render(0, None)).unwrap();
        assert!(value.as_object().unwrap().contains_key("callEntryOffset"));
        assert!(value["callEntryOffset"].is_null());
    }

    #[test]
    #[should_panic(expected = "call entry must name an instruction")]
    fn code_map_refuses_one_past_end_call_entry() {
        let mut map = CodeMapCapture::default();
        map.record_call_entry(64);
        map.validate_call_entry(64);
    }

    #[test]
    fn code_map_call_region_names_the_linked_target() {
        let mut map = CodeMapCapture::default();
        map.record(CodeRegion::call_structural(
            "machineCallTrampoline",
            20,
            28,
            7,
            2,
            19,
            Some(11),
        ));

        let value: serde_json::Value =
            serde_json::from_str(&map.render(0, None)).expect("valid code-map JSON");
        let region = &value["regions"][0];
        assert_eq!(region["functionId"], 7);
        assert_eq!(region["logicalPc"], 2);
        assert_eq!(region["bytePc"], 19);
        assert_eq!(region["callTargetFunctionId"], 11);
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

#[cfg(test)]
mod return_site_schema_tests {
    #[test]
    fn returns_reference_source_records_in_the_one_current_format() {
        use otter_vm::native_abi::{NO_FRAME_STATE, SafepointEntry, SafepointRecord};
        let mut record = SafepointRecord::window(17, NO_FRAME_STATE);
        record.call_pc = 7;
        let rendered = super::render_safepoints(
            &[record],
            &[
                SafepointEntry {
                    native_return_offset: 12,
                    safepoint_id: 17,
                },
                SafepointEntry {
                    native_return_offset: 28,
                    safepoint_id: 17,
                },
            ],
        );
        let value: serde_json::Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(value.as_object().unwrap().len(), 2);
        assert_eq!(value["records"][0]["id"], 17);
        assert_eq!(value["records"][0]["callPc"], 7);
        assert!(
            value["records"][0]["taggedLocations"]
                .as_array()
                .unwrap()
                .is_empty(),
            "a window record names no slot beyond the traced window"
        );
        assert_eq!(
            value["returnSites"],
            serde_json::json!([
                {"nativeReturnOffset":12,"safepointId":17},
                {"nativeReturnOffset":28,"safepointId":17},
            ])
        );
    }
}
