//! Accounted frozen code directories for independent isolate restores.
//!
//! # Contents
//! - Capture immutable ranges/compiler owners and freshly copied execution.
//! - Restore fresh local feedback/atoms on the supplied resource account.
//!
//! # Invariants
//! - Captured directories never observe later donor links or evictions.
//! - Compiler bytecode/source text share one immutable physical lease owner.
//!   Capture retains no donor payload, mutable executable or atom table.
//! - Every fresh execution/block/counter/table and directory owns its admitted
//!   physical bytes. Refusal drops the unpublished copy without publishing IDs.
//! - Restore resolves atom IDs only after all code admissions succeed.
//! - Function/property-site ranges, including charged tombstones, are preserved.
//!
//! # See also
//! - `crate::snapshot` owns heap capture and `crate::interp::restore` relocation.

use super::*;

pub(crate) struct CodeSpaceSnapshot {
    chunks: Box<[CapturedChunk]>,
    _directory_lease: ResourceLease,
}

struct CapturedChunk {
    function_base: u32,
    function_count: u32,
    property_ic_site_base: u32,
    property_ic_site_end: u32,
    retention: ChunkRetention,
    payload: Option<CapturedPayload>,
}

struct CapturedPayload {
    module: Arc<LinkedBytecode>,
    executable: Arc<ExecutableModule>,
    sources: crate::source_registry::SourceRegistry,
}

impl CodeSpace {
    pub(crate) fn capture(
        &self,
        account: &ResourceAccount,
    ) -> Result<CodeSpaceSnapshot, ResourceError> {
        let chunks = self.chunks();
        let bytes = (std::mem::size_of::<CodeSpaceSnapshot>() as u64).saturating_add(
            (chunks.len() as u64).saturating_mul(std::mem::size_of::<CapturedChunk>() as u64),
        );
        let mut lease = account.reserve_exact(ResourceClass::SourceModuleBytes, bytes)?;
        let mut captured = crate::executable::allocation::try_vec(chunks.len(), &mut lease)?;
        for chunk in chunks.iter() {
            let guard = chunk
                .payload
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let payload = guard
                .as_ref()
                .map(|payload| -> Result<CapturedPayload, ResourceError> {
                    Ok(CapturedPayload {
                        module: Arc::clone(&payload.module),
                        executable: Arc::new(payload.executable.fresh_isolate_copy(account)?),
                        sources: payload.sources.clone(),
                    })
                })
                .transpose()?;
            captured.push(CapturedChunk {
                function_base: chunk.function_base,
                function_count: chunk.function_count,
                property_ic_site_base: chunk.property_ic_site_base,
                property_ic_site_end: chunk.property_ic_site_end,
                retention: chunk.retention,
                payload,
            });
        }
        let captured = captured.into_boxed_slice();
        lease.resize(bytes)?;
        Ok(CodeSpaceSnapshot {
            chunks: captured,
            _directory_lease: lease,
        })
    }
}

impl CodeSpaceSnapshot {
    pub(crate) fn restore(
        &self,
        names: &mut crate::property_atom::NameInterner,
        account: &ResourceAccount,
    ) -> Result<Arc<CodeSpace>, ResourceError> {
        let space = Arc::new(CodeSpace::default());
        for chunk in &self.chunks {
            let payload = chunk
                .payload
                .as_ref()
                .map(|captured| -> Result<Arc<ChunkPayload>, ResourceError> {
                    let executable = Arc::new(captured.executable.fresh_isolate_copy(account)?);
                    let metadata_bytes = (std::mem::size_of::<ChunkPayload>() as u64)
                        .saturating_add(std::mem::size_of::<AtomTable>() as u64)
                        .saturating_add(AtomTable::allocation_bytes(&captured.module.constants));
                    let mut retained_lease =
                        account.reserve_exact(ResourceClass::SourceModuleBytes, metadata_bytes)?;
                    let atoms = Arc::new(AtomTable::from_constants(
                        &captured.module.constants,
                        &mut retained_lease,
                    )?);
                    let metadata_bytes = payload_metadata_bytes(&atoms);
                    retained_lease.resize(metadata_bytes)?;
                    let retained_bytes = metadata_bytes
                        .saturating_add(captured.module._lease.amount())
                        .saturating_add(executable.retained_bytes());
                    Ok(Arc::new(ChunkPayload {
                        module: Arc::clone(&captured.module),
                        executable,
                        atoms,
                        sources: captured.sources.clone(),
                        retained_bytes,
                        _retained_lease: retained_lease,
                    }))
                })
                .transpose()?;
            let restored = Arc::new(CodeChunk {
                function_base: chunk.function_base,
                function_count: chunk.function_count,
                property_ic_site_base: chunk.property_ic_site_base,
                property_ic_site_end: chunk.property_ic_site_end,
                retention: chunk.retention,
                payload: RwLock::new(payload),
                _directory_node_lease: account.reserve_exact(
                    ResourceClass::SourceModuleBytes,
                    std::mem::size_of::<CodeChunk>() as u64,
                )?,
            });
            publish_chunk(&space, restored, account)?;
        }
        space.resolve_atoms(names);
        Ok(space)
    }
}

#[cfg(test)]
#[path = "code_space/snapshot_tests.rs"]
mod tests;
