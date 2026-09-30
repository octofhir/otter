//! Frozen code directories for independent isolate restores.
//!
//! # Contents
//! Capture immutable ranges and admitted code; restore local execution tables.
//!
//! # Invariants
//! Captured directories never observe later donor links or evictions.
//! Every restored registry owns its feedback and atom tables. Only immutable
//! compiler bytecode is shared; no captured record retains an IC or GC handle.
//! Function and property-site ranges, including tombstones, are preserved.
//!
//! # See also
//! `crate::snapshot` owns heap capture and `crate::interp::restore` relocation.

use super::*;

pub(crate) struct CodeSpaceSnapshot {
    chunks: Box<[CapturedChunk]>,
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
    module: Arc<BytecodeModule>,
    executable: ExecutableModule,
}

impl CodeSpace {
    pub(crate) fn capture(&self) -> CodeSpaceSnapshot {
        CodeSpaceSnapshot {
            chunks: self
                .chunks()
                .iter()
                .map(|chunk| CapturedChunk {
                    function_base: chunk.function_base,
                    function_count: chunk.function_count,
                    property_ic_site_base: chunk.property_ic_site_base,
                    property_ic_site_end: chunk.property_ic_site_end,
                    retention: chunk.retention,
                    payload: chunk
                        .payload
                        .read()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .as_ref()
                        .map(|payload| CapturedPayload {
                            module: Arc::clone(&payload.module),
                            executable: payload.executable.fresh_isolate_copy(),
                        }),
                })
                .collect(),
        }
    }
}

impl CodeSpaceSnapshot {
    pub(crate) fn restore(&self, names: &crate::property_atom::NameInterner) -> Arc<CodeSpace> {
        let space = Arc::new(CodeSpace::default());
        let account = ResourceAccount::default();
        for chunk in &self.chunks {
            let payload = chunk.payload.as_ref().map(|captured| {
                let executable = Arc::new(captured.executable.fresh_isolate_copy());
                let atoms = Arc::new(AtomTable::from_constants(&captured.module.constants));
                atoms.resolve(names);
                let retained_bytes = chunk_retained_bytes(&captured.module, &executable, &atoms);
                let retained_lease = account
                    .reserve_exact(ResourceClass::SourceModuleBytes, retained_bytes)
                    .expect("addressable captured code fits an unlimited resource account");
                Arc::new(ChunkPayload {
                    module: Arc::clone(&captured.module),
                    executable,
                    atoms,
                    retained_bytes,
                    _retained_lease: retained_lease,
                })
            });
            if chunk.retention == ChunkRetention::Evictable
                && let Some(payload) = &payload
            {
                space
                    .evictable_retained_bytes
                    .fetch_add(payload.retained_bytes(), Ordering::Relaxed);
            }
            space
                .chunks
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(Arc::new(CodeChunk {
                    function_base: chunk.function_base,
                    function_count: chunk.function_count,
                    property_ic_site_base: chunk.property_ic_site_base,
                    property_ic_site_end: chunk.property_ic_site_end,
                    retention: chunk.retention,
                    payload: RwLock::new(payload),
                }));
        }
        space
    }
}
