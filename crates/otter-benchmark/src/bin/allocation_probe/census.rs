//! Cold ownership census at explicit script-probe boundaries.
//!
//! # Contents
//! - Distinct GC live, allocated-space, RSS and resource-ledger observations.
//! - Installed native code and generation lifecycle ownership.
//! - Concrete public representation sizes for architecture comparisons.
//!
//! # Invariants
//! - No census runs inside JavaScript execution, allocation or collection.
//! - Reported live bytes are reconciled only after an explicit full collection.
//! - Ledger classes overlap collector accounting and must never be added to it.
//! - Logical code bytes exclude mapping slack and retained side metadata.
//!
//! # See also
//! - `script` brackets the unchanged source and owns forced collection.

use std::error::Error;

use otter_runtime::{ResourceSnapshot, Runtime};
use serde_json::{Value, json};
use sysinfo::{ProcessesToUpdate, System};

pub(super) fn resources(snapshot: &ResourceSnapshot) -> Value {
    json!(
        snapshot
            .iter()
            .map(|entry| json!({
                "class": entry.class(),
                "current": entry.current(),
                "peak": entry.peak(),
                "rejections": entry.rejections(),
            }))
            .collect::<Vec<_>>()
    )
}

pub(super) fn capture(runtime: &mut Runtime, reconciled: bool) -> Result<Value, Box<dyn Error>> {
    let gc = runtime.heap_stats().clone();
    let spaces = runtime.heap_accounting_stats();
    let pid = sysinfo::get_current_pid()?;
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::Some(&[pid]), true);
    let rss = system.process(pid).map(sysinfo::Process::memory);
    let code = runtime.jit_code_residency();
    let generations = runtime.jit_code_generation_snapshot();
    Ok(json!({
        "rssBytes": rss,
        "liveBytesReconciled": reconciled,
        "reportedLiveCellBytes": gc.live_bytes,
        "reportedLiveObjects": gc.live_objects,
        "allocatedSpaceBytes": spaces.allocated_bytes,
        "reservedOffSlotBytes": spaces.reserved_bytes,
        "resources": resources(&runtime.resource_snapshot()),
        "byType": gc.by_type.iter().enumerate()
            .filter(|(_, row)| row.live_bytes != 0)
            .map(|(tag, row)| json!({"tag":tag,"liveBytes":row.live_bytes,"liveCount":row.live_count}))
            .collect::<Vec<_>>(),
        "code": {
            "installedOptimized": code.installed_optimized_bodies,
            "installedTemplateEntry": code.installed_entry_bodies,
            "installedTemplateOsr": code.installed_osr_bodies,
            "uniqueInstalled": code.unique_code_objects,
            "logicalInstalledBytes": code.code_bytes,
            "generations": generations.iter().map(|generation| json!({
                "id": generation.code_object_id,
                "functionId": generation.function_id,
                "tier": format!("{:?}", generation.tier),
                "lifecycle": format!("{:?}", generation.lifecycle),
                "linked": generation.linked,
                "currentEntry": generation.current_entry,
                "activeCount": generation.active_count,
                "dependencyCount": generation.dependencies.as_ref().map(Vec::len),
            })).collect::<Vec<_>>(),
        },
        "representationBytes": {
            "objectFixedBody": std::mem::size_of::<otter_vm::object::ObjectBody>(),
            "canonicalInstruction": std::mem::size_of::<otter_bytecode::WordInstruction>(),
            "executionFetchView": std::mem::size_of::<otter_vm::CodeBlockInstruction>(),
            "compileInstructionMetadata": std::mem::size_of::<otter_vm::JitInstructionMetadata>(),
            "instructionFeedback": std::mem::size_of::<otter_vm::feedback::InstructionFeedback>(),
        },
    }))
}
