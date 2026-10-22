//! Accounted frozen code directories for independent isolate restores.
//!
//! # Contents
//! - Capture immutable ranges/compiler owners and freshly copied execution.
//! - Restore fresh local feedback/atoms on the supplied resource account.
//! - Encode/decode the directory as part of the executable's startup image.
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
    alias: Option<u32>,
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
                alias: chunk.alias,
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
                        .saturating_add(captured.module.retained_bytes())
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
                alias: chunk.alias,
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

impl CodeSpaceSnapshot {
    /// Append the directory to a startup image: per chunk its id ranges,
    /// retention and alias, then its module bytes, lazy-record forwards and
    /// sources. Program-image source text is written as its displacement
    /// from `anchor`, other text verbatim.
    pub(crate) fn encode(&self, out: &mut otter_gc::image_codec::ImageWriter, anchor: usize) {
        out.len(self.chunks.len());
        for chunk in &self.chunks {
            out.u32(chunk.function_base);
            out.u32(chunk.function_count);
            out.u32(chunk.property_ic_site_base);
            out.u32(chunk.property_ic_site_end);
            out.u8(u8::from(chunk.retention == ChunkRetention::Evictable));
            out.u32(chunk.alias.unwrap_or(u32::MAX));
            let Some(payload) = &chunk.payload else {
                out.u8(0);
                continue;
            };
            out.u8(1);
            out.bytes(&otter_bytecode::binary::encode_module(
                payload.module.verified().module(),
            ));
            let count = payload.module.functions.len();
            out.len(count);
            for index in 0..count {
                out.u32(payload.executable.forward(index as u32).unwrap_or(u32::MAX));
            }
            let entries: Vec<_> = payload.sources.entries().collect();
            out.len(entries.len());
            for (url, text, prologue) in entries {
                out.str(url);
                out.u32(prologue);
                match text.program_image_text() {
                    Some(text) => {
                        out.u8(1);
                        out.u64((text.as_ptr() as usize).wrapping_sub(anchor) as u64);
                        out.len(text.len());
                    }
                    None => {
                        out.u8(0);
                        out.str(text);
                    }
                }
            }
        }
    }

    /// Read back a directory [`Self::encode`] wrote into this executable's
    /// startup image, with program-image text found at `anchor` plus its
    /// recorded displacement.
    ///
    /// # Safety
    /// The bytes must be the startup image this executable wrote, so every
    /// recorded displacement names program-image text of this executable.
    pub(crate) unsafe fn decode(
        input: &mut otter_gc::image_codec::ImageReader<'static>,
        anchor: usize,
        account: &ResourceAccount,
    ) -> Option<Self> {
        let count = input.len()?;
        let mut chunks = Vec::with_capacity(count);
        for _ in 0..count {
            let function_base = input.u32()?;
            let function_count = input.u32()?;
            let property_ic_site_base = input.u32()?;
            let property_ic_site_end = input.u32()?;
            let retention = match input.u8()? {
                0 => ChunkRetention::Pinned,
                1 => ChunkRetention::Evictable,
                _ => return None,
            };
            let alias = Some(input.u32()?).filter(|&alias| alias != u32::MAX);
            let payload = match input.u8()? {
                0 => None,
                1 => {
                    let verified = otter_bytecode::binary::decode_startup_image_module(
                        input.bytes()?,
                        function_base,
                    )
                    .ok()?;
                    let module = LinkedBytecode::new(verified, account).ok()?;
                    let mut executable =
                        ExecutableModule::lazy(Arc::clone(&module), property_ic_site_base, account)
                            .ok()?;
                    if let Some(alias) = alias {
                        executable = executable.with_alias(alias);
                    }
                    let forwards = input.len()?;
                    for index in 0..forwards {
                        let base = input.u32()?;
                        if base != u32::MAX {
                            executable.set_forward(index as u32, base);
                        }
                    }
                    let entries = input.len()?;
                    let mut sources = Vec::with_capacity(entries);
                    for _ in 0..entries {
                        let url = input.str()?.to_owned();
                        let prologue = input.u32()?;
                        let text = match input.u8()? {
                            1 => {
                                let address = anchor.wrapping_add(input.u64()? as usize);
                                let len = input.u32()? as usize;
                                // SAFETY: the caller guarantees the image
                                // names program-image text of this
                                // executable, which lives for the process.
                                let text: &'static str = unsafe {
                                    std::str::from_utf8_unchecked(std::slice::from_raw_parts(
                                        address as *const u8,
                                        len,
                                    ))
                                };
                                otter_resource::SharedSource::from_static(text)
                            }
                            0 => otter_resource::SharedSource::admit(
                                account,
                                input.str()?.to_owned(),
                            )
                            .ok()?,
                            _ => return None,
                        };
                        sources.push((url, text, prologue));
                    }
                    let sources =
                        crate::source_registry::SourceRegistry::from_entries(sources, account)
                            .ok()?;
                    Some(CapturedPayload {
                        module,
                        executable: Arc::new(executable),
                        sources,
                    })
                }
                _ => return None,
            };
            chunks.push(CapturedChunk {
                function_base,
                function_count,
                property_ic_site_base,
                property_ic_site_end,
                retention,
                alias,
                payload,
            });
        }
        let bytes = (std::mem::size_of::<CodeSpaceSnapshot>() as u64).saturating_add(
            (chunks.len() as u64).saturating_mul(std::mem::size_of::<CapturedChunk>() as u64),
        );
        let lease = account
            .reserve_exact(ResourceClass::SourceModuleBytes, bytes)
            .ok()?;
        Some(Self {
            chunks: chunks.into_boxed_slice(),
            _directory_lease: lease,
        })
    }
}

#[cfg(test)]
#[path = "code_space/snapshot_tests.rs"]
mod tests;
