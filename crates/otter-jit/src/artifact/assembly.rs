//! Deterministic, address-redacted AArch64 assembly artifacts.
//!
//! # Contents
//! - [`render`] decodes finalized `code.bin` bytes into an annotated text
//!   listing with code-relative offsets and stable branch labels.
//! - Relocation, code-map, deopt, and safepoint annotations are rendered from
//!   the already-built artifact DTOs.
//! - Template inline annotations expose guard/body/deopt ranges and compact
//!   virtual-register-to-scratch assignments.
//! - Generated direct-call annotations expose exact target generation, tier,
//!   receiver mode, register shape, and stack reservations without exposing
//!   its entry-cell address.
//!
//! # Invariants
//! - Every native location is an offset relative to the matching `code.bin`.
//! - Address materializations are replaced by one symbolic relocation line;
//!   neither encoded MOV-wide words nor their immediate chunks are printed.
//! - Direct branches name deterministic local labels rather than process
//!   addresses or raw displacements.
//! - Decoder failures preserve exact bytes through a `.word` fallback.
//! - This renderer runs only after explicit artifact capture. It never walks
//!   compiler IR, VM frames, or the GC heap.
//!
//! # See also
//! - [`super::relocation`] for validated symbolic address sites.
//! - [`super::CodeMapCapture`] for emission-time native/source correlation.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use otter_vm::{
    JitArtifactMetadata, JitDebugTarget, JitDebugTier, SafepointRecord, TaggedLocationKind,
    deopt::DeoptTable,
};
use yaxpeax_arch::{Arch, Decoder, U8Reader};
use yaxpeax_arm::armv8::a64::ARMv8;

use super::relocation::{
    CollectionFeedbackKind, CollectionHeapComponent, DirectBranch, DirectBranchKind,
    PropertyIcAccess, RelocationTarget, TemplateOperandArena, TemplateOperandRole,
    ValidatedRelocation, ValidatedRelocations, decode_direct_branch,
};
use super::{
    CodeMapCapture, CodeRegion, InlineScratchEntryArtifact, InlineScratchLayoutArtifact,
    OsrCodeEntry,
};

const HEADER: &str = "; otter jit aarch64 assembly\n";

/// Render one finalized AArch64 code object and its existing metadata.
pub(super) fn render(
    metadata: &JitArtifactMetadata,
    code: &[u8],
    entry_offset: usize,
    code_map: &CodeMapCapture,
    relocations: &ValidatedRelocations,
    deopt_table: Option<&DeoptTable>,
    safepoints: &[SafepointRecord],
) -> String {
    let mut output = String::with_capacity(code.len().saturating_mul(32));
    output.push_str(HEADER);
    output.push_str("; offset-basis=code.bin\n");
    writeln!(output, "; target={}", metadata.target).expect("writing to String cannot fail");
    writeln!(output, "; architecture={}", metadata.architecture)
        .expect("writing to String cannot fail");
    writeln!(output, "; operating-system={}", metadata.operating_system)
        .expect("writing to String cannot fail");
    writeln!(output, "; tier={}", tier_name(metadata.tier)).expect("writing to String cannot fail");
    writeln!(output, "; function-id={}", metadata.function_id)
        .expect("writing to String cannot fail");
    writeln!(output, "; function-name={:?}", metadata.function_name)
        .expect("writing to String cannot fail");
    writeln!(output, "; module={:?}", metadata.module).expect("writing to String cannot fail");
    writeln!(output, "; code-object-id={}", metadata.code_object_id)
        .expect("writing to String cannot fail");
    writeln!(
        output,
        "; compile-target={}",
        compile_target(metadata.entry)
    )
    .expect("writing to String cannot fail");
    writeln!(output, "; entry-offset=+0x{entry_offset:08x}")
        .expect("writing to String cannot fail");
    writeln!(output, "; code-bytes={}", code.len()).expect("writing to String cannot fail");

    render_deopt_summary(&mut output, deopt_table);
    render_safepoint_summary(&mut output, safepoints);
    output.push('\n');

    let labels = collect_labels(code, entry_offset, code_map, relocations);
    let region_starts = region_starts(code_map);
    let osr_starts = osr_starts(code_map);
    let mut relocation_index = 0usize;
    let mut offset = 0usize;

    while offset < code.len() {
        if labels.contains(&offset) {
            writeln!(output, "L{offset:08x}:").expect("writing to String cannot fail");
        }
        if let Some(entries) = osr_starts.get(&offset) {
            for entry in entries {
                render_osr_annotation(&mut output, entry);
            }
        }
        if let Some(regions) = region_starts.get(&offset) {
            for region in regions {
                render_region_annotation(&mut output, region, deopt_table);
            }
        }

        if let Some(relocation) = relocations.records.get(relocation_index)
            && relocation.start_offset as usize == offset
        {
            render_relocation(&mut output, relocation);
            offset = relocation.end_offset as usize;
            relocation_index += 1;
            continue;
        }

        let word = read_word(code, offset);
        let instruction = if let Some(branch) = decode_direct_branch(word, offset) {
            render_branch(branch)
        } else {
            decode_instruction(&code[offset..offset + 4])
                .unwrap_or_else(|| format!(".word 0x{word:08x}"))
        };
        writeln!(output, "+0x{offset:08x}: {word:08x}  {instruction}")
            .expect("writing to String cannot fail");
        offset += 4;
    }

    output
}

fn tier_name(tier: JitDebugTier) -> &'static str {
    match tier {
        JitDebugTier::Template => "template",
        JitDebugTier::Optimizing => "optimizing",
    }
}

fn compile_target(target: JitDebugTarget) -> String {
    match target {
        JitDebugTarget::Entry => "entry".to_string(),
        JitDebugTarget::SyncEntry => "sync-entry".to_string(),
        JitDebugTarget::Osr { pc } => format!("osr pc={pc}"),
    }
}

fn render_deopt_summary(output: &mut String, table: Option<&DeoptTable>) {
    let Some(table) = table else {
        output.push_str("; deopt-exits=0\n");
        return;
    };
    writeln!(output, "; deopt-exits={}", table.len()).expect("writing to String cannot fail");
    for (exit_id, state) in table.entries().iter().enumerate() {
        let slot_count = state
            .frames
            .iter()
            .map(|frame| frame.slots.len())
            .sum::<usize>();
        if let Some(frame) = state.frames.last() {
            writeln!(
                output,
                "; deopt exit={exit_id} frames={} innermost-function={} innermost-byte-pc={} slots={slot_count}",
                state.frames.len(),
                frame.function_id,
                frame.byte_pc,
            )
            .expect("writing to String cannot fail");
        } else {
            writeln!(
                output,
                "; deopt exit={exit_id} frames=0 innermost=unavailable slots=0"
            )
            .expect("writing to String cannot fail");
        }
    }
}

fn render_safepoint_summary(output: &mut String, safepoints: &[SafepointRecord]) {
    writeln!(output, "; safepoints={}", safepoints.len()).expect("writing to String cannot fail");
    let mut records: Vec<_> = safepoints.iter().collect();
    records.sort_by_key(|record| record.id);
    for record in records {
        write!(
            output,
            "; safepoint id={} native-offset=unavailable frame-state={} tagged=",
            record.id, record.frame_state
        )
        .expect("writing to String cannot fail");
        if record.tagged_locations.is_empty() {
            output.push_str("none");
        } else {
            for (index, location) in record.tagged_locations.iter().enumerate() {
                if index != 0 {
                    output.push(',');
                }
                write!(
                    output,
                    "{}:{}",
                    tagged_location_name(location.kind),
                    location.index
                )
                .expect("writing to String cannot fail");
            }
        }
        output.push('\n');
    }
}

fn tagged_location_name(kind: TaggedLocationKind) -> &'static str {
    match kind {
        TaggedLocationKind::FrameSlot => "frameSlot",
        TaggedLocationKind::MachineRegister => "machineRegister",
        TaggedLocationKind::SpillSlot => "spillSlot",
    }
}

fn region_starts(code_map: &CodeMapCapture) -> BTreeMap<usize, Vec<&CodeRegion>> {
    let mut starts = BTreeMap::<usize, Vec<&CodeRegion>>::new();
    for region in &code_map.regions {
        starts
            .entry(region.start_offset as usize)
            .or_default()
            .push(region);
    }
    for regions in starts.values_mut() {
        regions.sort_by(|left, right| {
            (
                left.end_offset,
                left.kind,
                left.block,
                left.target_block,
                left.inline_frame,
                left.function_id,
                left.logical_pc,
                left.byte_pc,
                left.operation_index,
                left.operation.as_deref(),
                left.deopt_exit_id,
            )
                .cmp(&(
                    right.end_offset,
                    right.kind,
                    right.block,
                    right.target_block,
                    right.inline_frame,
                    right.function_id,
                    right.logical_pc,
                    right.byte_pc,
                    right.operation_index,
                    right.operation.as_deref(),
                    right.deopt_exit_id,
                ))
        });
    }
    starts
}

fn osr_starts(code_map: &CodeMapCapture) -> BTreeMap<usize, Vec<&OsrCodeEntry>> {
    let mut starts = BTreeMap::<usize, Vec<&OsrCodeEntry>>::new();
    for entry in &code_map.osr_entries {
        starts
            .entry(entry.start_offset as usize)
            .or_default()
            .push(entry);
    }
    for entries in starts.values_mut() {
        entries.sort_by_key(|entry| (entry.logical_pc, entry.end_offset));
    }
    starts
}

fn render_osr_annotation(output: &mut String, entry: &OsrCodeEntry) {
    writeln!(
        output,
        "  ; osr-entry pc={} range=+0x{:08x}..+0x{:08x}",
        entry.logical_pc, entry.start_offset, entry.end_offset
    )
    .expect("writing to String cannot fail");
}

fn render_region_annotation(
    output: &mut String,
    region: &CodeRegion,
    deopt_table: Option<&DeoptTable>,
) {
    write!(
        output,
        "  ; region kind={} range=+0x{:08x}..+0x{:08x}",
        region.kind, region.start_offset, region.end_offset
    )
    .expect("writing to String cannot fail");
    if let Some(block) = region.block {
        write!(output, " block={block}").expect("writing to String cannot fail");
    }
    if let Some(target_block) = region.target_block {
        write!(output, " target-block={target_block}").expect("writing to String cannot fail");
    }
    if let Some(inline_frame) = region.inline_frame {
        write!(output, " inline-frame={inline_frame}").expect("writing to String cannot fail");
    }
    if let Some(site) = region.inline_site {
        write!(
            output,
            " inline-site=caller:{}:pc:{}:byte:{} receiver-property={}",
            site.caller_function_id, site.logical_pc, site.byte_pc, site.has_receiver_property,
        )
        .expect("writing to String cannot fail");
    }
    if let Some(function_id) = region.function_id {
        write!(output, " function={function_id}").expect("writing to String cannot fail");
    }
    if let Some(direct_call) = region.direct_call {
        write!(
            output,
            " call-kind={} call-target-function={} call-target-code-object-id={} call-target-tier={} call-this-mode={} call-callee-native-frame-bytes={} call-linkage-bytes={} call-reserved-stack-bytes={} call-callee-register-count={}",
            direct_call.call_kind.name(),
            direct_call.target_function_id,
            direct_call.target_code_object_id,
            direct_call.target_tier.name(),
            direct_call.this_mode.name(),
            direct_call.callee_native_frame_bytes,
            direct_call.linkage_bytes,
            direct_call.reserved_stack_bytes,
            direct_call.callee_register_count,
        )
        .expect("writing to String cannot fail");
    }
    if let Some(method_guard) = &region.method_guard {
        write!(
            output,
            " method-guard-receiver-register={} method-guard-function={} method-guard-receiver-shape={} method-guard-prototype-shapes={:?} method-guard-value-byte={}",
            method_guard.receiver_register,
            method_guard.method_function_id,
            method_guard.receiver_shape,
            method_guard.prototype_shapes,
            method_guard.method_value_byte,
        )
        .expect("writing to String cannot fail");
    }
    if let Some(logical_pc) = region.logical_pc {
        write!(output, " pc={logical_pc}").expect("writing to String cannot fail");
    }
    if let Some(byte_pc) = region.byte_pc {
        write!(output, " byte-pc={byte_pc}").expect("writing to String cannot fail");
    }
    if let Some(operation_index) = region.operation_index {
        write!(output, " operation-index={operation_index}")
            .expect("writing to String cannot fail");
    }
    if let Some(operation) = &region.operation {
        write!(output, " tier-op={operation:?}").expect("writing to String cannot fail");
    }
    if let Some(layout) = &region.inline_scratch_layout {
        render_inline_scratch_annotation(output, layout);
    }
    if let Some(exit_id) = region.deopt_exit_id {
        write!(output, " deopt-exit={exit_id}").expect("writing to String cannot fail");
        if let Some(state) = deopt_table.and_then(|table| table.entries().get(exit_id as usize)) {
            let slots = state
                .frames
                .iter()
                .map(|frame| frame.slots.len())
                .sum::<usize>();
            write!(
                output,
                " deopt-frames={} deopt-slots={slots}",
                state.frames.len()
            )
            .expect("writing to String cannot fail");
        }
    }
    output.push('\n');
}

fn render_inline_scratch_annotation(output: &mut String, layout: &InlineScratchLayoutArtifact) {
    write!(
        output,
        " parameters={} virtual-registers={} scratch-slots={} slot-bytes={} stack-alignment={} scratch-bytes={} offset-basis={} register-slots=[",
        layout.parameter_count,
        layout.virtual_register_count,
        layout.scratch_slot_count,
        layout.slot_bytes,
        layout.stack_alignment_bytes,
        layout.scratch_bytes,
        layout.offset_basis,
    )
    .expect("writing to String cannot fail");
    for (index, slot) in layout.register_slots.iter().enumerate() {
        if index != 0 {
            output.push(',');
        }
        match slot {
            Some(slot) => write!(output, "r{index}:s{slot}"),
            None => write!(output, "r{index}:-"),
        }
        .expect("writing to String cannot fail");
    }
    output.push_str("] receiver-slot=");
    match layout.receiver_slot {
        Some(slot) => {
            write!(output, "s{slot}").expect("writing to String cannot fail");
        }
        None => output.push('-'),
    }
    output.push_str(" entry-values=[");
    for (index, entry) in layout.entry_values.iter().enumerate() {
        if index != 0 {
            output.push(',');
        }
        match *entry {
            InlineScratchEntryArtifact::Argument {
                argument,
                register,
                slot,
            } => write!(output, "arg{argument}->r{register}:s{slot}"),
            InlineScratchEntryArtifact::Receiver { slot } => {
                write!(output, "receiver:s{slot}")
            }
            InlineScratchEntryArtifact::Undefined { register, slot } => {
                write!(output, "undefined->r{register}:s{slot}")
            }
        }
        .expect("writing to String cannot fail");
    }
    output.push(']');
}

fn collect_labels(
    code: &[u8],
    entry_offset: usize,
    code_map: &CodeMapCapture,
    relocations: &ValidatedRelocations,
) -> BTreeSet<usize> {
    let mut labels = BTreeSet::new();
    if entry_offset < code.len() {
        labels.insert(entry_offset);
    }
    for entry in &code_map.osr_entries {
        if entry.start_offset < code.len() as u64 {
            labels.insert(entry.start_offset as usize);
        }
    }

    let mut relocation_index = 0usize;
    let mut offset = 0usize;
    while offset < code.len() {
        if let Some(relocation) = relocations.records.get(relocation_index)
            && relocation.start_offset as usize == offset
        {
            offset = relocation.end_offset as usize;
            relocation_index += 1;
            continue;
        }
        if let Some(branch) = decode_direct_branch(read_word(code, offset), offset)
            && let Ok(target) = usize::try_from(branch.target)
            && target < code.len()
        {
            labels.insert(target);
        }
        offset += 4;
    }
    labels
}

fn render_relocation(output: &mut String, relocation: &ValidatedRelocation) {
    let register = register_name(relocation.register, relocation.width_bits == 64);
    let bytes = relocation.end_offset - relocation.start_offset;
    writeln!(
        output,
        "+0x{:08x}: relocation {register}, {} ; encoded-bytes={bytes} redacted",
        relocation.start_offset,
        symbolic_target(&relocation.target)
    )
    .expect("writing to String cannot fail");
}

fn symbolic_target(target: &RelocationTarget) -> String {
    match target {
        RelocationTarget::RuntimeStub {
            id,
            name,
            signature,
        } => format!("runtimeStub(id={id},name={name:?},signature={signature:?})"),
        RelocationTarget::GcCageBase => "gcCageBase".to_string(),
        RelocationTarget::PropertyIcCell { access, ordinal } => format!(
            "propertyIcCell(access={},ordinal={ordinal})",
            match access {
                PropertyIcAccess::Load => "load",
                PropertyIcAccess::Store => "store",
            }
        ),
        RelocationTarget::TemplateOperandSlice {
            arena,
            role,
            start,
            len,
        } => format!(
            "templateOperandSlice(arena={},role={},start={start},len={len})",
            operand_arena_name(*arena),
            operand_role_name(*role)
        ),
        RelocationTarget::OptimizedMathArguments {
            inline_frame,
            logical_pc,
            len,
        } => {
            format!("optimizedMathArguments(inlineFrame={inline_frame},pc={logical_pc},len={len})")
        }
        RelocationTarget::CollectionHeapReference {
            component,
            feedback_kind,
            byte_pc,
            runtime_stub_id,
        } => format!(
            "collectionHeapReference(component={},feedback={},bytePc={byte_pc},runtimeStubId={runtime_stub_id})",
            heap_component_name(*component),
            feedback_kind_name(*feedback_kind)
        ),
        RelocationTarget::CollectionBuiltinFunction {
            feedback_kind,
            byte_pc,
            runtime_stub_id,
        } => format!(
            "collectionBuiltinFunction(feedback={},bytePc={byte_pc},runtimeStubId={runtime_stub_id})",
            feedback_kind_name(*feedback_kind)
        ),
        RelocationTarget::StaticNativeBuiltinFunction { target, byte_pc } => {
            format!("staticNativeBuiltinFunction(target={target:?},bytePc={byte_pc})")
        }
        RelocationTarget::DirectCallEntryCell {
            byte_pc,
            direct_call,
        } => format!(
            "directCallEntryCell(callerBytePc={byte_pc},callKind={},targetFunction={},targetCodeObjectId={},targetTier={},thisMode={},calleeNativeFrameBytes={},linkageBytes={},reservedStackBytes={},calleeRegisterCount={})",
            direct_call.call_kind.name(),
            direct_call.target_function_id,
            direct_call.target_code_object_id,
            direct_call.target_tier.name(),
            direct_call.this_mode.name(),
            direct_call.callee_native_frame_bytes,
            direct_call.linkage_bytes,
            direct_call.reserved_stack_bytes,
            direct_call.callee_register_count,
        ),
    }
}

fn operand_arena_name(arena: TemplateOperandArena) -> &'static str {
    match arena {
        TemplateOperandArena::Registers => "registers",
        TemplateOperandArena::Indices => "indices",
    }
}

fn operand_role_name(role: TemplateOperandRole) -> &'static str {
    match role {
        TemplateOperandRole::ClosureParents => "closureParents",
        TemplateOperandRole::NewArrayElements => "newArrayElements",
        TemplateOperandRole::MathArguments => "mathArguments",
        TemplateOperandRole::ConstructArguments => "constructArguments",
    }
}

fn heap_component_name(component: CollectionHeapComponent) -> &'static str {
    match component {
        CollectionHeapComponent::Prototype => "prototype",
        CollectionHeapComponent::PrototypeShape => "prototypeShape",
    }
}

fn feedback_kind_name(kind: CollectionFeedbackKind) -> &'static str {
    match kind {
        CollectionFeedbackKind::Leaf => "leaf",
        CollectionFeedbackKind::Alloc => "alloc",
    }
}

fn render_branch(branch: DirectBranch) -> String {
    let label = format!("L{:08x}", branch.target);
    match branch.kind {
        DirectBranchKind::B => format!("b {label}"),
        DirectBranchKind::Bl => format!("bl {label}"),
        DirectBranchKind::BCond { condition } => {
            format!("b.{} {label}", condition_name(condition))
        }
        DirectBranchKind::Cbz {
            is_64_bit,
            register,
        } => format!("cbz {}, {label}", register_name(register, is_64_bit)),
        DirectBranchKind::Cbnz {
            is_64_bit,
            register,
        } => format!("cbnz {}, {label}", register_name(register, is_64_bit)),
        DirectBranchKind::Tbz { bit, register } => format!(
            "tbz {}, #{bit}, {label}",
            register_name(register, bit >= 32)
        ),
        DirectBranchKind::Tbnz { bit, register } => format!(
            "tbnz {}, #{bit}, {label}",
            register_name(register, bit >= 32)
        ),
    }
}

fn condition_name(condition: u8) -> &'static str {
    const NAMES: [&str; 16] = [
        "eq", "ne", "cs", "cc", "mi", "pl", "vs", "vc", "hi", "ls", "ge", "lt", "gt", "le", "al",
        "nv",
    ];
    NAMES[usize::from(condition & 0xf)]
}

fn register_name(register: u8, is_64_bit: bool) -> String {
    if register == 31 {
        if is_64_bit {
            "xzr".to_string()
        } else {
            "wzr".to_string()
        }
    } else if is_64_bit {
        format!("x{register}")
    } else {
        format!("w{register}")
    }
}

fn decode_instruction(bytes: &[u8]) -> Option<String> {
    let decoder = <ARMv8 as Arch>::Decoder::default();
    let mut reader = U8Reader::new(bytes);
    decoder
        .decode(&mut reader)
        .ok()
        .map(|instruction| instruction.to_string())
}

fn read_word(code: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(
        code[offset..offset + 4]
            .try_into()
            .expect("validated AArch64 instruction range"),
    )
}

#[cfg(test)]
mod tests {
    use otter_vm::{
        JitDebugTarget, JitDebugTier, TaggedLocation,
        deopt::{DeoptFrame, DeoptLocation, DeoptRepr, DeoptSlot, FrameState},
    };

    use super::*;
    use crate::artifact::{
        DirectCallArtifact, DirectCallKindArtifact, DirectCallThisModeArtifact,
        DirectCallTierArtifact, relocation::RelocationCapture,
    };

    const NOP: u32 = 0xd503_201f;
    const RET: u32 = 0xd65f_03c0;

    fn words(words: &[u32]) -> Vec<u8> {
        words.iter().flat_map(|word| word.to_le_bytes()).collect()
    }

    fn metadata() -> JitArtifactMetadata {
        JitArtifactMetadata {
            target: "aarch64-apple-darwin".to_string(),
            architecture: "aarch64".to_string(),
            operating_system: "macos".to_string(),
            tier: JitDebugTier::Template,
            function_id: 7,
            function_name: "fixture".to_string(),
            module: "fixture.js".to_string(),
            code_object_id: 9,
            entry: JitDebugTarget::Entry,
            bytecode_bytes: 12,
            code_bytes: 8,
        }
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

    fn movz(register: u8, immediate: u16, shift_bits: u8) -> u32 {
        0xd280_0000
            | (u32::from(shift_bits / 16) << 21)
            | (u32::from(immediate) << 5)
            | u32::from(register)
    }

    fn movk(register: u8, immediate: u16, shift_bits: u8) -> u32 {
        0xf280_0000
            | (u32::from(shift_bits / 16) << 21)
            | (u32::from(immediate) << 5)
            | u32::from(register)
    }

    #[test]
    fn annotations_are_deterministic_and_come_from_existing_metadata() {
        let code = words(&[NOP, RET]);
        let relocations = RelocationCapture::new(true).render(&code).unwrap();
        let mut code_map = CodeMapCapture::default();
        code_map.record(CodeRegion::instruction(
            0,
            4,
            None,
            None,
            7,
            2,
            19,
            Some(0),
            "Move { dst: 1, src: 0 }".to_string(),
        ));
        code_map.record(CodeRegion::deopt(4, 8, 0, 3));
        code_map.record_osr(2, 4, 8);
        let deopt = DeoptTable::from_states(vec![FrameState {
            frames: vec![DeoptFrame {
                function_id: 7,
                byte_pc: 19,
                slots: vec![DeoptSlot {
                    location: DeoptLocation::Register(3),
                    repr: DeoptRepr::Tagged,
                }]
                .into_boxed_slice(),
            }]
            .into_boxed_slice(),
        }]);
        let safepoints = [SafepointRecord {
            id: 3,
            frame_state: 0,
            tagged_locations: vec![TaggedLocation::frame_slot(1)],
        }];

        let first = render(
            &metadata(),
            &code,
            0,
            &code_map,
            &relocations.validated,
            Some(&deopt),
            &safepoints,
        );
        let second = render(
            &metadata(),
            &code,
            0,
            &code_map,
            &relocations.validated,
            Some(&deopt),
            &safepoints,
        );
        assert_eq!(first, second);
        assert!(first.starts_with(HEADER));
        assert!(first.contains("; offset-basis=code.bin"));
        assert!(first.contains("; tier=template"));
        assert!(first.contains("pc=2"));
        assert!(first.contains("tier-op=\"Move { dst: 1, src: 0 }\""));
        assert!(first.contains("deopt exit=0 frames=1"));
        assert!(first.contains("deopt-exit=0 deopt-frames=1 deopt-slots=1"));
        assert!(first.contains("safepoint id=3 native-offset=unavailable"));
        assert!(first.contains("tagged=frameSlot:1"));
        assert!(first.contains("L00000000:"));
    }

    #[test]
    fn direct_call_region_names_caller_and_target() {
        let mut output = String::new();
        render_region_annotation(
            &mut output,
            &CodeRegion::call_structural(
                "directCallGuard",
                4,
                12,
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
            ),
            None,
        );
        assert_eq!(
            output,
            "  ; region kind=directCallGuard range=+0x00000004..+0x0000000c function=7 call-kind=plain call-target-function=11 call-target-code-object-id=29 call-target-tier=optimizing call-this-mode=sloppyGlobal call-callee-native-frame-bytes=160 call-linkage-bytes=112 call-reserved-stack-bytes=272 call-callee-register-count=6 pc=2 byte-pc=19\n"
        );
    }

    #[test]
    fn relocation_lines_are_symbolic_and_redact_address_chunks() {
        let code = words(&[movz(16, 0xdead, 0), movk(16, 0xbeef, 16), RET]);
        let mut capture = RelocationCapture::new(true);
        capture.record_mov_wide(
            0,
            8,
            16,
            RelocationTarget::RuntimeStub {
                id: 1,
                name: "jit_backedge_poll",
                signature: "poll1",
            },
        );
        let relocations = capture.render(&code).unwrap();
        let assembly = render(
            &metadata(),
            &code,
            0,
            &CodeMapCapture::default(),
            &relocations.validated,
            None,
            &[],
        );
        let line = assembly
            .lines()
            .find(|line| line.starts_with("+0x00000000:"))
            .expect("relocation line");
        let (_, right) = line.split_once(": ").expect("offset separator");
        assert!(right.starts_with("relocation "));
        assert!(!right.contains("0x"));
        assert!(!assembly.contains("dead"));
        assert!(!assembly.contains("beef"));
        assert!(!assembly.contains("movz"));
        assert!(!assembly.contains("movk"));
        assert!(line.contains("runtimeStub(id=1"));
    }

    #[test]
    fn direct_call_relocation_names_exact_generation_and_stack_contract() {
        let code = words(&[movz(16, 0xdead, 0), RET]);
        let mut capture = RelocationCapture::new(true);
        capture.record_mov_wide(
            0,
            4,
            16,
            RelocationTarget::DirectCallEntryCell {
                byte_pc: 19,
                direct_call: DirectCallArtifact {
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
            },
        );
        let relocations = capture.render(&code).unwrap();
        let assembly = render(
            &metadata(),
            &code,
            0,
            &CodeMapCapture::default(),
            &relocations.validated,
            None,
            &[],
        );
        assert!(assembly.starts_with("; otter jit aarch64 assembly\n"));
        assert!(assembly.contains(
            "directCallEntryCell(callerBytePc=19,callKind=plain,targetFunction=11,targetCodeObjectId=29,targetTier=optimizing,thisMode=sloppyGlobal,calleeNativeFrameBytes=160,linkageBytes=112,reservedStackBytes=272,calleeRegisterCount=6)"
        ));
    }

    #[test]
    fn direct_branches_use_code_relative_labels() {
        let target = 0x20_i32;
        let code = words(&[
            b(target, false),
            b(target - 4, true),
            b_cond(target - 8, 0),
            cb(target - 12, false, true, 3),
            tb(target - 16, true, 47, 4),
            NOP,
            NOP,
            NOP,
            RET,
        ]);
        let relocations = RelocationCapture::new(true).render(&code).unwrap();
        let assembly = render(
            &metadata(),
            &code,
            0,
            &CodeMapCapture::default(),
            &relocations.validated,
            None,
            &[],
        );
        assert!(assembly.contains("L00000020:"));
        assert!(assembly.contains("b L00000020"));
        assert!(assembly.contains("bl L00000020"));
        assert!(assembly.contains("b.eq L00000020"));
        assert!(assembly.contains("cbz x3, L00000020"));
        assert!(assembly.contains("tbnz x4, #47, L00000020"));
        assert!(!assembly.contains("$+"));
        assert!(!assembly.contains("$-"));
    }

    #[test]
    fn decoder_failure_uses_word_fallback() {
        let invalid = [u32::MAX, 0x0123_4567, 0x0fff_ffff]
            .into_iter()
            .find(|word| decode_instruction(&word.to_le_bytes()).is_none())
            .expect("fixture includes an undecodable AArch64 word");
        let code = words(&[invalid]);
        let relocations = RelocationCapture::new(true).render(&code).unwrap();
        let assembly = render(
            &metadata(),
            &code,
            0,
            &CodeMapCapture::default(),
            &relocations.validated,
            None,
            &[],
        );
        assert!(assembly.contains(&format!(".word 0x{invalid:08x}")));
    }
}
