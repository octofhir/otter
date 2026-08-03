//! Isolate snapshot capture — the writer half of "don't run the
//! bootstrap, restore its result".
//!
//! A built isolate's durable state splits three ways:
//!
//! - **The heap image.** Old-space pages, captured verbatim by
//!   [`otter_gc::GcHeap::capture_old_space`]; every body in them is
//!   self-contained (the audit in `otter-gc/src/self_contained.rs`
//!   enforces zero escaping references and zero foreign ownership for
//!   the bootstrap graph).
//! - **Fixed roots.** Interpreter fields holding heap handles whose
//!   count and order are the same on every isolate —
//!   [`crate::Interpreter::visit_snapshot_roots`] walks them; the
//!   capture stores the sequence, a restore writes it back relocated.
//! - **Keyed side state.** Isolate-local structures the heap references
//!   by identifier rather than by handle: the property-name atom table
//!   (ids are baked into shapes and executable code), the global
//!   lexicals (name → cell), and — future work — per-type records for
//!   the payloads that cannot ride a page image at all (host objects,
//!   compiled regexps).
//!
//! `ShapeRuntime`'s side tables are deliberately *not* captured: they
//! are caches over heap state (`trace_roots` shows exactly which
//! cells), and a restore rebuilds them from the restored shape bodies.
//!
//! # Invariants
//!
//! - Capture requires an empty nursery; a full build ends with one, and
//!   the underlying page capture rejects anything else.
//! - `atom_names` is the interner's table in id order: restoring the
//!   list re-mints identical [`crate::property_atom::AtomId`]s.
//! - `global_lexicals` is sorted by name so two captures of the same
//!   build compare equal.
//!
//! # See also
//!
//! - `otter-gc/src/heap_image.rs` — pages and relocation.
//! - `scratchpad/PLAN_BOOTSTRAP_SNAPSHOT.md` — the plan this executes.

use otter_gc::raw::RawGc;

/// One dynamic-native closure carried by a snapshot, keyed by the
/// host-ref index the captured bodies name it with.
#[derive(Clone)]
pub enum DynamicNativePayload {
    /// `Send + Sync` embedder/runtime closure.
    Shared(std::sync::Arc<crate::native_function::NativeFn>),
    /// Isolate-local VM helper closure.
    Local(std::sync::Arc<crate::native_function::LocalNativeFn>),
}

impl DynamicNativePayload {
    /// Wrap a `Send + Sync` closure — what a blob resolver returns for
    /// a name it reconstructs.
    pub fn shared<F>(call: F) -> Self
    where
        F: for<'rt> Fn(
                &mut crate::NativeCtx<'rt>,
                &[crate::Value],
                &[crate::Value],
            ) -> Result<crate::Value, crate::native_function::NativeError>
            + Send
            + Sync
            + 'static,
    {
        Self::Shared(std::sync::Arc::new(call))
    }
}

/// Everything a restore needs that the page image alone does not carry.
pub struct IsolateSnapshot {
    /// The old generation, verbatim.
    pub image: otter_gc::HeapImage,
    /// The isolate's code space, shared by reference. Restored closure
    /// bodies name their bytecode by function id inside it. In-process
    /// only; the build-time snapshot pairs with embedded bytecode.
    pub(crate) code_space: std::sync::Arc<crate::code_space::CodeSpace>,
    /// Dynamic-native closures at their host-ref indices.
    pub(crate) dynamic_natives: Vec<(u32, DynamicNativePayload)>,
    /// Heap handles from the fixed-shape root walk, in walk order.
    pub fixed_roots: Vec<RawGc>,
    /// Every external-reference address in index order, so a restore
    /// rebuilds the table with identical indices (slid by the loader's
    /// image move when the restoring process differs).
    pub external_ref_addrs: Vec<usize>,
    /// The property-name atom table, in id order.
    pub atom_names: Vec<Box<str>>,
    /// Global lexical bindings: name, cell handle, `is_const`.
    pub global_lexicals: Vec<(Box<str>, RawGc, bool)>,
    /// Per-body regexp pattern records in live-walk order — the
    /// serializer contract for the one payload the page image cannot
    /// carry (the compiled matcher and its foreign-owned text).
    pub regexp_payloads: Vec<(Vec<u16>, String)>,
    /// Per-body array-sidecar descriptor-flag records in live-walk
    /// order: `(key, writable, enumerable, configurable)` per entry.
    pub array_sidecar_flags: Vec<Vec<(String, bool, bool, bool)>>,
    /// The capture process's next shape id. A restoring process bumps
    /// its own counter past this so freshly minted shape ids never
    /// collide with the image's — shape ids key the property caches.
    pub next_shape_id: u64,
}

/// Format marker for [`IsolateSnapshot::to_bytes`]. Not versioned —
/// the cache key already changes with every rebuild, so a stale blob
/// is never found, only an absent one.
const BLOB_MAGIC: &[u8] = b"otter-isolate-snapshot\0";

impl IsolateSnapshot {
    /// Serialize to a flat byte stream a later process of the same
    /// binary reads back with [`Self::from_bytes`]. Dynamic-native
    /// closures serialize as display names; the reader re-creates the
    /// payloads through its resolver.
    #[must_use]
    pub fn to_bytes(&self, dynamic_names: &[(u32, String)]) -> Vec<u8> {
        let image = self.image.to_bytes();
        let mut out = Vec::with_capacity(image.len() + 64 * 1024);
        out.extend_from_slice(BLOB_MAGIC);
        push_bytes(&mut out, &image);
        out.extend_from_slice(&(self.external_ref_addrs.len() as u32).to_le_bytes());
        for &addr in &self.external_ref_addrs {
            out.extend_from_slice(&(addr as u64).to_le_bytes());
        }
        out.extend_from_slice(&(self.fixed_roots.len() as u32).to_le_bytes());
        for root in &self.fixed_roots {
            out.extend_from_slice(&root.0.to_le_bytes());
        }
        out.extend_from_slice(&(self.atom_names.len() as u32).to_le_bytes());
        for name in &self.atom_names {
            push_str(&mut out, name);
        }
        out.extend_from_slice(&(self.global_lexicals.len() as u32).to_le_bytes());
        for (name, cell, is_const) in &self.global_lexicals {
            push_str(&mut out, name);
            out.extend_from_slice(&cell.0.to_le_bytes());
            out.push(u8::from(*is_const));
        }
        let chunks = self.code_space.chunk_snapshots();
        out.extend_from_slice(&(chunks.len() as u32).to_le_bytes());
        for (function_base, ic_base, module) in &chunks {
            out.extend_from_slice(&function_base.to_le_bytes());
            out.extend_from_slice(&ic_base.to_le_bytes());
            push_bytes(&mut out, &otter_bytecode::binary::encode_module(module));
        }
        out.extend_from_slice(&(dynamic_names.len() as u32).to_le_bytes());
        for (index, name) in dynamic_names {
            out.extend_from_slice(&index.to_le_bytes());
            push_str(&mut out, name);
        }
        out.extend_from_slice(&(self.regexp_payloads.len() as u32).to_le_bytes());
        for (pattern_utf16, source) in &self.regexp_payloads {
            out.extend_from_slice(&(pattern_utf16.len() as u32).to_le_bytes());
            for unit in pattern_utf16 {
                out.extend_from_slice(&unit.to_le_bytes());
            }
            push_str(&mut out, source);
        }
        out.extend_from_slice(&(self.array_sidecar_flags.len() as u32).to_le_bytes());
        for records in &self.array_sidecar_flags {
            out.extend_from_slice(&(records.len() as u32).to_le_bytes());
            for (key, writable, enumerable, configurable) in records {
                push_str(&mut out, key);
                out.push(u8::from(*writable));
                out.push(u8::from(*enumerable));
                out.push(u8::from(*configurable));
            }
        }
        out.extend_from_slice(&self.next_shape_id.to_le_bytes());
        out
    }

    /// Decode a stream [`Self::to_bytes`] wrote. `resolve` supplies
    /// each dynamic-native closure by its captured display name; a
    /// name it cannot supply, like any structural mismatch, yields
    /// `None` and the caller bootstraps instead.
    #[must_use]
    pub fn from_bytes(
        bytes: &[u8],
        resolve: &mut dyn FnMut(&str) -> Option<DynamicNativePayload>,
    ) -> Option<Self> {
        let mut r = BlobReader::new(bytes);
        if r.take(BLOB_MAGIC.len())? != BLOB_MAGIC {
            return None;
        }
        let image = otter_gc::HeapImage::from_bytes(r.bytes_block()?)?;
        let mut external_ref_addrs = Vec::new();
        for _ in 0..r.u32()? {
            external_ref_addrs.push(r.u64()? as usize);
        }
        let mut fixed_roots = Vec::new();
        for _ in 0..r.u32()? {
            fixed_roots.push(RawGc(r.u32()?));
        }
        let mut atom_names = Vec::new();
        for _ in 0..r.u32()? {
            atom_names.push(Box::from(r.str_block()?));
        }
        let mut global_lexicals = Vec::new();
        for _ in 0..r.u32()? {
            let name = Box::from(r.str_block()?);
            let cell = RawGc(r.u32()?);
            let is_const = r.u8()? != 0;
            global_lexicals.push((name, cell, is_const));
        }
        let code_space = std::sync::Arc::new(crate::code_space::CodeSpace::default());
        for _ in 0..r.u32()? {
            let function_base = r.u32()?;
            let ic_base = r.u32()?;
            let module = otter_bytecode::binary::decode_module(r.bytes_block()?)?;
            code_space.link_restored_chunk(module, function_base, ic_base);
        }
        let mut dynamic_natives = Vec::new();
        for _ in 0..r.u32()? {
            let index = r.u32()?;
            let name = r.str_block()?;
            dynamic_natives.push((index, resolve(name)?));
        }
        let mut regexp_payloads = Vec::new();
        for _ in 0..r.u32()? {
            let unit_count = r.u32()? as usize;
            let mut pattern_utf16 = Vec::with_capacity(unit_count.min(64 * 1024));
            for _ in 0..unit_count {
                pattern_utf16.push(u16::from_le_bytes(r.take(2)?.try_into().unwrap()));
            }
            let source = r.str_block()?.to_owned();
            regexp_payloads.push((pattern_utf16, source));
        }
        let mut array_sidecar_flags = Vec::new();
        for _ in 0..r.u32()? {
            let record_count = r.u32()? as usize;
            let mut records = Vec::with_capacity(record_count.min(1024));
            for _ in 0..record_count {
                let key = r.str_block()?.to_owned();
                let writable = r.u8()? != 0;
                let enumerable = r.u8()? != 0;
                let configurable = r.u8()? != 0;
                records.push((key, writable, enumerable, configurable));
            }
            array_sidecar_flags.push(records);
        }
        let next_shape_id = r.u64()?;
        if !r.is_empty() {
            return None;
        }
        Some(Self {
            image,
            code_space,
            dynamic_natives,
            fixed_roots,
            external_ref_addrs,
            atom_names,
            global_lexicals,
            regexp_payloads,
            array_sidecar_flags,
            next_shape_id,
        })
    }
}

fn push_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    out.extend_from_slice(bytes);
}

fn push_str(out: &mut Vec<u8>, s: &str) {
    push_bytes(out, s.as_bytes());
}

/// Bounds-checked little-endian cursor for [`IsolateSnapshot::from_bytes`].
struct BlobReader<'a> {
    bytes: &'a [u8],
}

impl<'a> BlobReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.bytes.len() < n {
            return None;
        }
        let (head, rest) = self.bytes.split_at(n);
        self.bytes = rest;
        Some(head)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }

    fn u32(&mut self) -> Option<u32> {
        self.take(4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
    }

    fn u64(&mut self) -> Option<u64> {
        self.take(8)
            .map(|b| u64::from_le_bytes(b.try_into().unwrap()))
    }

    fn bytes_block(&mut self) -> Option<&'a [u8]> {
        let len = self.u32()? as usize;
        self.take(len)
    }

    fn str_block(&mut self) -> Option<&'a str> {
        std::str::from_utf8(self.bytes_block()?).ok()
    }

    fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }
}

impl crate::Interpreter {
    /// Capture everything a restore needs from this isolate.
    ///
    /// # Errors
    /// Propagates [`otter_gc::ImageError`]; a fresh build leaves the
    /// nursery empty, so a capture taken right after one succeeds.
    pub fn capture_isolate_snapshot(&self) -> Result<IsolateSnapshot, otter_gc::ImageError> {
        let image = self.capture_heap_image()?;
        let code_space = self.snapshot_code_space();
        let dynamic_natives = crate::native_function::snapshot_dynamic_natives(self.gc_heap());
        let fixed_roots = self.capture_snapshot_roots();
        let external_ref_addrs = self.gc_heap().external_refs().addrs().to_vec();
        let mut regexp_payloads: Vec<(Vec<u16>, String)> = Vec::new();
        self.gc_heap()
            .for_each_live_payload::<crate::regexp::JsRegExpBody, _>(|_space, body| {
                regexp_payloads.push((body.pattern_utf16.clone(), body.source.clone()));
            });
        let mut array_sidecar_with_content = false;
        let mut array_sidecar_flags: Vec<Vec<(String, bool, bool, bool)>> = Vec::new();
        self.gc_heap()
            .for_each_live_payload::<crate::array::ArrayExoticSlots, _>(|_space, body| {
                if !body.is_empty_for_snapshot() {
                    array_sidecar_with_content = true;
                    eprintln!(
                        "snapshot capture: array sidecar holds {}",
                        body.snapshot_content_summary()
                    );
                }
                array_sidecar_flags.push(body.snapshot_property_flags());
            });
        if array_sidecar_with_content {
            return Err(otter_gc::ImageError::ForeignPayloadNotSerializable {
                type_name: "ArrayExoticSlots",
            });
        }
        let next_shape_id = crate::object::snapshot_next_shape_id();
        let atom_names = self.snapshot_atom_names();
        let mut global_lexicals: Vec<(Box<str>, RawGc, bool)> = self
            .snapshot_global_lexicals()
            .into_iter()
            .map(|(name, cell, is_const)| (name, RawGc(cell.offset()), is_const))
            .collect();
        global_lexicals.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(IsolateSnapshot {
            image,
            code_space,
            dynamic_natives,
            fixed_roots,
            external_ref_addrs,
            atom_names,
            global_lexicals,
            regexp_payloads,
            array_sidecar_flags,
            next_shape_id,
        })
    }
}
