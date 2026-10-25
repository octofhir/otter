//! Accounted frozen code directories for independent isolate restores.
//!
//! # Contents
//! - Capture immutable ranges/compiler owners and freshly copied execution.
//! - Restore fresh local feedback/atoms on the supplied resource account.
//! - Encode/decode the directory as part of the executable's startup image;
//!   a decoded chunk stays dormant until a function in it is resolved.
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

enum CapturedPayload {
    /// A linked chunk: its immutable module, a fresh execution copy, and
    /// the atom ids its constants resolved to.
    Linked {
        module: Arc<LinkedBytecode>,
        executable: Arc<ExecutableModule>,
        atom_ids: Arc<[u32]>,
        sources: crate::source_registry::SourceRegistry,
    },
    /// A startup-image chunk no function of which was resolved yet.
    Dormant(DormantChunk),
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
            let payload = match guard.as_ref() {
                Some(payload) => Some(CapturedPayload::Linked {
                    module: Arc::clone(&payload.module),
                    executable: Arc::new(payload.executable.fresh_isolate_copy(account)?),
                    atom_ids: payload.atoms.ids().collect(),
                    sources: payload.sources.clone(),
                }),
                None => chunk
                    .dormant
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone()
                    .map(CapturedPayload::Dormant),
            };
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
            let (payload, dormant) = match &chunk.payload {
                Some(CapturedPayload::Linked {
                    module,
                    executable,
                    sources,
                    ..
                }) => {
                    let executable = Arc::new(executable.fresh_isolate_copy(account)?);
                    let metadata_bytes = (std::mem::size_of::<ChunkPayload>() as u64)
                        .saturating_add(std::mem::size_of::<AtomTable>() as u64)
                        .saturating_add(AtomTable::allocation_bytes(&module.constants));
                    let mut retained_lease =
                        account.reserve_exact(ResourceClass::SourceModuleBytes, metadata_bytes)?;
                    let atoms = Arc::new(AtomTable::from_constants(
                        &module.constants,
                        &mut retained_lease,
                    )?);
                    let metadata_bytes = payload_metadata_bytes(&atoms);
                    retained_lease.resize(metadata_bytes)?;
                    let retained_bytes = metadata_bytes
                        .saturating_add(module.retained_bytes())
                        .saturating_add(executable.retained_bytes());
                    let payload = Arc::new(ChunkPayload {
                        module: Arc::clone(module),
                        executable,
                        atoms,
                        sources: sources.clone(),
                        retained_bytes,
                        _retained_lease: retained_lease,
                    });
                    (Some(payload), None)
                }
                // The restoring isolate's account charges what it decodes.
                Some(CapturedPayload::Dormant(dormant)) => (
                    None,
                    Some(DormantChunk {
                        account: account.clone(),
                        ..dormant.clone()
                    }),
                ),
                None => (None, None),
            };
            let restored = Arc::new(CodeChunk {
                function_base: chunk.function_base,
                function_count: chunk.function_count,
                property_ic_site_base: chunk.property_ic_site_base,
                property_ic_site_end: chunk.property_ic_site_end,
                retention: chunk.retention,
                alias: chunk.alias,
                payload: RwLock::new(payload),
                dormant: Mutex::new(dormant),
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
            match payload {
                CapturedPayload::Linked {
                    module,
                    executable,
                    atom_ids,
                    sources,
                } => {
                    out.bytes(&otter_bytecode::binary::encode_module(
                        module.verified().module(),
                    ));
                    let count = module.functions.len();
                    out.len(count);
                    for index in 0..count {
                        out.u32(executable.forward(index as u32).unwrap_or(u32::MAX));
                    }
                    out.len(atom_ids.len());
                    atom_ids.iter().for_each(|&id| out.u32(id));
                    out.len(count);
                    for function in &module.functions {
                        encode_header(out, FunctionHeader::of(function));
                    }
                    encode_sources(out, sources, anchor);
                }
                CapturedPayload::Dormant(dormant) => {
                    out.bytes(dormant.bytes);
                    out.len(dormant.forwards.len());
                    dormant.forwards.iter().for_each(|&base| out.u32(base));
                    out.len(dormant.atom_ids.len());
                    dormant.atom_ids.iter().for_each(|&id| out.u32(id));
                    out.len(dormant.headers.len());
                    dormant
                        .headers
                        .iter()
                        .for_each(|&header| encode_header(out, header));
                    encode_sources(out, &dormant.sources, anchor);
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
                    // The module stays encoded until a function in it is
                    // first resolved.
                    let bytes = input.bytes()?;
                    let forwards = decode_u32s(input)?;
                    let atom_ids = decode_u32s(input)?;
                    let headers = (0..input.len()?)
                        .map(|_| {
                            Some(FunctionHeader {
                                id: input.u32()?,
                                param_count: u16::try_from(input.u32()?).ok()?,
                                register_count: u16::try_from(input.u32()?).ok()?,
                                call_flags: input.u32()?,
                            })
                        })
                        .collect::<Option<Arc<[FunctionHeader]>>>()?;
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
                    Some(CapturedPayload::Dormant(DormantChunk {
                        bytes,
                        forwards,
                        atom_ids,
                        headers,
                        sources,
                        account: account.clone(),
                    }))
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

fn encode_header(out: &mut otter_gc::image_codec::ImageWriter, header: FunctionHeader) {
    out.u32(header.id);
    out.u32(u32::from(header.param_count));
    out.u32(u32::from(header.register_count));
    out.u32(header.call_flags);
}

/// A chunk's sources: program-image text as its displacement from
/// `anchor`, other text verbatim.
fn encode_sources(
    out: &mut otter_gc::image_codec::ImageWriter,
    sources: &crate::source_registry::SourceRegistry,
    anchor: usize,
) {
    let entries: Vec<_> = sources.entries().collect();
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

fn decode_u32s(input: &mut otter_gc::image_codec::ImageReader<'static>) -> Option<Arc<[u32]>> {
    (0..input.len()?).map(|_| input.u32()).collect()
}

#[cfg(test)]
#[path = "code_space/snapshot_tests.rs"]
mod tests;
