//! Isolate snapshot capture, in process and as an executable's startup
//! image.
//!
//! A built isolate's captured state splits three ways:
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
//!   Iterator and function-kind prototype caches are captured in their own
//!   fixed-size root arrays so restore can install them without allocation.
//! - **Keyed side state.** Isolate-local structures the heap references
//!   by identifier rather than by handle: the property-name atom table
//!   (ids are baked into shapes and executable code), the global
//!   lexicals (name → cell), the global symbol registry (key → symbol),
//!   and per-type records for the payloads that cannot ride a page image
//!   at all (compiled regexps, indexed array descriptors).
//!
//! The heap-owned shape interner's side tables are deliberately not captured:
//! a restore rebuilds canonical ids, transitions, provisional descendants and
//! key roots from the relocated shape bodies. Current-turn pins start empty.
//! [`IsolateSnapshot::encode`] / [`IsolateSnapshot::decode`] carry the
//! snapshot as the startup image of the executable that wrote it; it never
//! crosses an executable or Rust type-layout boundary.
//!
//! # Contents
//!
//! - [`IsolateSnapshot`] — the opaque capture-produced carrier.
//! - [`crate::Interpreter::capture_isolate_snapshot`] — capture entry point.
//! - [`crate::Interpreter::from_isolate_snapshot`] — safe restore entry point.
//!
//! # Invariants
//!
//! - Capture requires an empty nursery; a full build ends with one, and
//!   the underlying page capture rejects anything else.
//! - `atom_names` is the interner's table in id order: restoring the
//!   list re-mints identical [`crate::property_atom::AtomId`]s.
//! - `global_lexicals` is sorted by name so two captures of the same
//!   build compare equal.
//! - `registered_symbols` keeps registration order, so `Symbol.for` and
//!   `Symbol.keyFor` answer a restored isolate as they answered the
//!   captured one.
//! - `realm_context_base` names the chunk the realm's host scripts ran in:
//!   a restored isolate runs no script before host events need that
//!   context, so the restore rebuilds it from the code space.
//! - Code capture admits independent mutable execution tables and directory
//!   metadata on the current resource account; immutable compiler/source owners
//!   retain their existing shared leases without pinning donor payloads.
//! - All restoration-capable fields are crate-private. Public methods expose
//!   owned-data diagnostics only, never raw heap handles or mutable carriers.
//! - Capture refuses a heap holding a body that owns storage outside the
//!   heap unless its type severs that ownership on restore or the snapshot
//!   rebuilds it from a record, and a host object whose payload type the
//!   host does not give back after a restore.
//!
//! # See also
//!
//! - `otter-gc/src/heap_image.rs` — pages and relocation.
//! - `otter-gc/src/external_refs.rs` — process-local static addresses.

use otter_gc::raw::RawGc;

pub(crate) const ITERATOR_PROTOTYPE_ROOT_COUNT: usize = 14;

/// One dynamic-native closure carried by a snapshot, keyed by the
/// host-ref index the captured bodies name it with.
#[derive(Clone)]
pub(crate) enum DynamicNativePayload {
    /// `Send + Sync` embedder/runtime closure.
    Shared(std::sync::Arc<crate::native_function::NativeFn>),
    /// Isolate-local VM helper closure.
    Local(std::sync::Arc<crate::native_function::LocalNativeFn>),
}

/// Everything a restore needs beyond the page image.
///
/// Instances can only be produced by [`crate::Interpreter`]. The fields remain
/// private outside the VM so embedders cannot forge raw roots, page contents,
/// or side-state records.
pub struct IsolateSnapshot {
    /// The old generation, verbatim.
    pub(crate) image: otter_gc::HeapImage,
    /// Frozen admitted code and ranges. Restores own fresh execution state;
    /// captured closures retain their function IDs within that directory.
    pub(crate) code_space: crate::code_space::snapshot::CodeSpaceSnapshot,
    /// Dynamic-native closures at their host-ref indices.
    pub(crate) dynamic_natives: Vec<(u32, DynamicNativePayload)>,
    /// Heap handles from the fixed-shape root walk, in walk order.
    pub(crate) fixed_roots: Vec<RawGc>,
    /// Active and default-realm iterator prototype handles, in field order.
    pub(crate) iterator_prototype_roots: [RawGc; ITERATOR_PROTOTYPE_ROOT_COUNT],
    /// Function-kind constructor/prototype handles, in field order.
    pub(crate) function_kind_prototype_roots:
        [RawGc; crate::function_kind::FunctionKindPrototypes::SNAPSHOT_ROOT_COUNT],
    /// Every external-reference address in index order, so a restore
    /// rebuilds the table with identical indices.
    pub(crate) external_ref_addrs: Vec<usize>,
    /// The property-name atom table, in id order.
    pub(crate) atom_names: Vec<Box<str>>,
    /// Global lexical bindings: name, cell handle, `is_const`.
    pub(crate) global_lexicals: Vec<(Box<str>, RawGc, bool)>,
    /// The global symbol registry: key and symbol handle, in registration
    /// order.
    pub(crate) registered_symbols: Vec<(Box<str>, RawGc)>,
    /// Per-body regexp pattern records, by body handle. These rebuild the
    /// compiled matcher and foreign-owned text after page relocation.
    pub(crate) regexp_payloads: Vec<(RawGc, Vec<u16>, String)>,
    /// Per-body indexed Array descriptor records, by sidecar handle.
    /// Managed named-property bags remain in the relocated page image.
    pub(crate) array_indexed_records: Vec<(RawGc, crate::array::ArrayIndexedSnapshot)>,
    /// The source isolate's next shape id. A restoring isolate bumps
    /// the process counter past this so freshly minted shape ids never
    /// collide with the image's — shape ids key the property caches.
    pub(crate) next_shape_id: u64,
    /// First function id of the chunk the realm's host scripts ran in, which
    /// host events run their native callbacks against.
    pub(crate) realm_context_base: Option<u32>,
    /// Distance from the capturing process's [`startup_image_anchor`] to
    /// this process's: what every program-image address in the image moved
    /// by. Zero for an in-process capture.
    pub(crate) image_slide: isize,
}

/// The function every startup-image displacement is measured from: the
/// executable loads as one unit, so its address moves exactly as far as
/// every other function and constant of the program image does.
#[inline(never)]
pub fn startup_image_anchor() {}

fn anchor_address() -> usize {
    startup_image_anchor as *const () as usize
}

/// How a startup image names one dynamic native: its display name, and
/// whether it is an isolate-local VM helper or a host closure.
#[derive(Debug, Clone)]
pub struct StartupNativeName {
    /// The host-reference index the image's bodies call it by.
    pub index: u32,
    /// The callable's display name.
    pub name: String,
    /// An isolate-local VM helper rather than a host closure.
    pub local: bool,
}

impl IsolateSnapshot {
    /// Borrow captured atom names in atom-id order.
    #[must_use]
    pub fn atom_names(&self) -> &[Box<str>] {
        &self.atom_names
    }

    /// Number of handles captured by the fixed root walk.
    #[must_use]
    pub const fn fixed_root_count(&self) -> usize {
        self.fixed_roots.len()
    }

    /// Iterate global lexical names in deterministic name order.
    pub fn global_lexical_names(&self) -> impl ExactSizeIterator<Item = &str> {
        self.global_lexicals
            .iter()
            .map(|(name, _, _)| name.as_ref())
    }

    /// Number of heap bodies captured in the image.
    #[must_use]
    pub fn object_count(&self) -> u64 {
        self.image.object_count()
    }
}

impl crate::Interpreter {
    /// Capture everything a restore needs from this isolate.
    ///
    /// # Errors
    /// Propagates [`otter_gc::ImageError`]; a fresh build leaves the
    /// nursery empty, so a capture taken right after one succeeds.
    pub fn capture_isolate_snapshot(&self) -> Result<IsolateSnapshot, otter_gc::ImageError> {
        // A body that owns storage outside the heap restores only when its
        // type severs that ownership on restore, or this snapshot rebuilds it
        // from a record (a regexp's compiled pattern). Anything else would
        // restore as a second owner of the writer's memory.
        let table = self.gc_heap().trace_table();
        let mut unrestorable: Option<&'static str> = None;
        self.gc_heap().for_each_live_body(|_space, body| {
            let tag = body.type_tag();
            if unrestorable.is_none()
                && table.owns_outside_storage(tag)
                && table.get_sever_restored(tag).is_none()
                && tag != crate::regexp::REGEXP_BODY_TYPE_TAG
            {
                unrestorable = Some(table.name(tag).unwrap_or("?"));
            }
        });
        if let Some(type_name) = unrestorable {
            return Err(otter_gc::ImageError::ForeignPayloadNotCapturable { type_name });
        }
        // A restore severs every host payload: a host object reaches the
        // image only when the host gives its payload type back afterwards.
        let mut undeclared_host_data: Option<&'static str> = None;
        self.gc_heap()
            .for_each_live_payload::<crate::object::ExoticSlots, _>(|_space, sidecar| {
                if let Some(payload) = sidecar.host_data_type()
                    && !self.host_services.restores_host_data(payload)
                {
                    undeclared_host_data = undeclared_host_data.or(sidecar.host_data_name());
                }
            });
        if let Some(type_name) = undeclared_host_data {
            return Err(otter_gc::ImageError::ForeignPayloadNotCapturable { type_name });
        }
        let image = self.capture_heap_image()?;
        let code_space = self.snapshot_code_space()?;
        let dynamic_natives = crate::native_function::snapshot_dynamic_natives(self.gc_heap());
        let fixed_roots = self.capture_snapshot_roots();
        let mut iterator_prototype_roots = [RawGc::NULL; ITERATOR_PROTOTYPE_ROOT_COUNT];
        for (slot, object) in iterator_prototype_roots[..7].iter_mut().zip([
            self.array_iterator_prototype.get(),
            self.map_iterator_prototype.get(),
            self.set_iterator_prototype.get(),
            self.string_iterator_prototype.get(),
            self.regexp_string_iterator_prototype.get(),
            self.iterator_helper_prototype.get(),
            self.wrap_for_valid_iterator_prototype.get(),
        ]) {
            *slot = object.map_or(RawGc::NULL, crate::object::JsObject::raw);
        }
        for (slot, root) in iterator_prototype_roots[7..]
            .iter_mut()
            .zip(&self.default_realm_iterator_prototypes)
        {
            *slot = root.get().map_or(RawGc::NULL, crate::object::JsObject::raw);
        }
        let function_kind_prototype_roots = self.function_kind_prototypes.snapshot_roots();
        let external_ref_addrs = self.gc_heap().external_refs().addresses().to_vec();
        let mut regexp_payloads: Vec<(RawGc, Vec<u16>, String)> = Vec::new();
        let mut array_sidecar_with_content = false;
        let mut array_indexed_records: Vec<(RawGc, crate::array::ArrayIndexedSnapshot)> =
            Vec::new();
        self.gc_heap().for_each_live_body(|_space, body| {
            if let Some(regexp) = body.downcast::<crate::regexp::JsRegExpBody>() {
                regexp_payloads.push((
                    body.raw(),
                    regexp.pattern.pattern_utf16.to_vec(),
                    regexp.pattern.source.to_string(),
                ));
            } else if let Some(sidecar) = body.downcast::<crate::array::ArrayExoticSlots>() {
                if !sidecar.is_empty_for_snapshot() {
                    array_sidecar_with_content = true;
                    eprintln!(
                        "snapshot capture: array sidecar holds {}",
                        sidecar.snapshot_content_summary()
                    );
                }
                array_indexed_records.push((body.raw(), sidecar.indexed_snapshot()));
            }
        });
        if array_sidecar_with_content {
            return Err(otter_gc::ImageError::ForeignPayloadNotCapturable {
                type_name: "ArrayExoticSlots",
            });
        }
        let next_shape_id = crate::object::snapshot_next_shape_id();
        let realm_context_base = self
            .realm_context
            .as_ref()
            .map(crate::ExecutionContext::function_base);
        let atom_names = self.snapshot_atom_names();
        let mut global_lexicals: Vec<(Box<str>, RawGc, bool)> = self
            .snapshot_global_lexicals()
            .into_iter()
            .map(|(name, cell, is_const)| (name, RawGc(cell.offset()), is_const))
            .collect();
        global_lexicals.sort_by(|a, b| a.0.cmp(&b.0));
        let registered_symbols = self.symbol_registry().snapshot_entries();
        Ok(IsolateSnapshot {
            image_slide: 0,
            image,
            code_space,
            dynamic_natives,
            fixed_roots,
            iterator_prototype_roots,
            function_kind_prototype_roots,
            external_ref_addrs,
            atom_names,
            global_lexicals,
            registered_symbols,
            regexp_payloads,
            array_indexed_records,
            next_shape_id,
            realm_context_base,
        })
    }
}

impl crate::Interpreter {
    /// The dynamic natives this isolate's bodies call, by name, in index
    /// order, as a startup image of it names them.
    #[must_use]
    pub fn startup_native_names(&self, snapshot: &IsolateSnapshot) -> Vec<StartupNativeName> {
        let heap_names = crate::native_function::dynamic_native_names(self.gc_heap());
        snapshot.dynamic_native_names(&heap_names)
    }
}

impl IsolateSnapshot {
    /// The dynamic natives the image's bodies call, by name, in index order.
    #[must_use]
    fn dynamic_native_names(&self, heap_names: &[(u32, String)]) -> Vec<StartupNativeName> {
        self.dynamic_natives
            .iter()
            .map(|(index, payload)| StartupNativeName {
                index: *index,
                name: heap_names
                    .iter()
                    .find(|(named, _)| named == index)
                    .map(|(_, name)| name.clone())
                    .unwrap_or_default(),
                local: matches!(payload, DynamicNativePayload::Local(_)),
            })
            .collect()
    }

    /// Append everything but the dynamic-native closures to a startup
    /// image; those are written as `natives`, re-created by name when the
    /// image is read.
    pub fn encode(
        &self,
        out: &mut otter_gc::image_codec::ImageWriter,
        natives: &[StartupNativeName],
    ) {
        let anchor = anchor_address();
        out.u64(anchor as u64);
        self.image.encode(out);
        self.code_space.encode(out, anchor);
        out.len(natives.len());
        for native in natives {
            out.u32(native.index);
            out.str(&native.name);
            out.u8(u8::from(native.local));
        }
        let raw_list = |out: &mut otter_gc::image_codec::ImageWriter, raws: &[RawGc]| {
            out.len(raws.len());
            for raw in raws {
                out.u32(raw.0);
            }
        };
        raw_list(out, &self.fixed_roots);
        raw_list(out, &self.iterator_prototype_roots);
        raw_list(out, &self.function_kind_prototype_roots);
        out.len(self.external_ref_addrs.len());
        for &address in &self.external_ref_addrs {
            out.u64(address.wrapping_sub(anchor) as u64);
        }
        out.len(self.atom_names.len());
        for name in &self.atom_names {
            out.str(name);
        }
        out.len(self.global_lexicals.len());
        for (name, cell, is_const) in &self.global_lexicals {
            out.str(name);
            out.u32(cell.0);
            out.u8(u8::from(*is_const));
        }
        out.len(self.registered_symbols.len());
        for (key, symbol) in &self.registered_symbols {
            out.str(key);
            out.u32(symbol.0);
        }
        out.len(self.regexp_payloads.len());
        for (body, pattern, source) in &self.regexp_payloads {
            out.u32(body.0);
            out.units(pattern);
            out.str(source);
        }
        out.len(self.array_indexed_records.len());
        for (sidecar, record) in &self.array_indexed_records {
            out.u32(sidecar.0);
            out.u8(u8::from(record.length_writable));
            out.len(record.property_flags.len());
            for (key, flags) in &record.property_flags {
                out.u32(*key);
                out.u8(flags.bits());
            }
        }
        out.u64(self.next_shape_id);
        out.u32(self.realm_context_base.unwrap_or(u32::MAX));
    }

    /// Read back an image [`Self::encode`] wrote, its dynamic natives
    /// re-created through `resolve` (host closures) and the VM's own helper
    /// table (isolate-local ones). `None` when the stream is not such an
    /// image or a named native has no maker.
    ///
    /// # Safety
    /// The bytes must be the startup image this same executable wrote: every
    /// page byte is trusted as a body this build laid out, and every recorded
    /// displacement names a function or text of this executable.
    pub unsafe fn decode(
        input: &mut otter_gc::image_codec::ImageReader<'static>,
        resolve: &mut dyn FnMut(&str) -> Option<std::sync::Arc<crate::native_function::NativeFn>>,
        account: &otter_resource::ResourceAccount,
    ) -> Option<Self> {
        let anchor = anchor_address();
        let captured_anchor = input.u64()? as usize;
        let image_slide = anchor.wrapping_sub(captured_anchor) as isize;
        // SAFETY: forwarded from this function's contract.
        let image = unsafe { otter_gc::HeapImage::decode(input) }?;
        // SAFETY: as above.
        let code_space = unsafe {
            crate::code_space::snapshot::CodeSpaceSnapshot::decode(input, anchor, account)
        }?;
        let count = input.len()?;
        let mut dynamic_natives = Vec::with_capacity(count);
        for _ in 0..count {
            let index = input.u32()?;
            let name = input.str()?;
            let payload = match input.u8()? {
                0 => DynamicNativePayload::Shared(resolve(name)?),
                1 => {
                    DynamicNativePayload::Local(crate::native_function::local_dynamic_native(name)?)
                }
                _ => return None,
            };
            dynamic_natives.push((index, payload));
        }
        let raw_list = |input: &mut otter_gc::image_codec::ImageReader<'static>| {
            let len = input.len()?;
            (0..len)
                .map(|_| input.u32().map(RawGc))
                .collect::<Option<Vec<_>>>()
        };
        let fixed_roots = raw_list(input)?;
        let iterator_prototype_roots = raw_list(input)?.try_into().ok()?;
        let function_kind_prototype_roots = raw_list(input)?.try_into().ok()?;
        let len = input.len()?;
        let external_ref_addrs = (0..len)
            .map(|_| {
                input
                    .u64()
                    .map(|offset| anchor.wrapping_add(offset as usize))
            })
            .collect::<Option<Vec<_>>>()?;
        let len = input.len()?;
        let atom_names = (0..len)
            .map(|_| input.str().map(Box::<str>::from))
            .collect::<Option<Vec<_>>>()?;
        let len = input.len()?;
        let mut global_lexicals = Vec::with_capacity(len);
        for _ in 0..len {
            global_lexicals.push((
                Box::<str>::from(input.str()?),
                RawGc(input.u32()?),
                input.u8()? != 0,
            ));
        }
        let len = input.len()?;
        let mut registered_symbols = Vec::with_capacity(len);
        for _ in 0..len {
            registered_symbols.push((Box::<str>::from(input.str()?), RawGc(input.u32()?)));
        }
        let len = input.len()?;
        let mut regexp_payloads = Vec::with_capacity(len);
        for _ in 0..len {
            regexp_payloads.push((RawGc(input.u32()?), input.units()?, input.str()?.to_owned()));
        }
        let len = input.len()?;
        let mut array_indexed_records = Vec::with_capacity(len);
        for _ in 0..len {
            let sidecar = RawGc(input.u32()?);
            let length_writable = input.u8()? != 0;
            let flags = input.len()?;
            let mut property_flags = Vec::with_capacity(flags);
            for _ in 0..flags {
                property_flags.push((
                    input.u32()?,
                    crate::object::PropertyFlags::from_bits(input.u8()?),
                ));
            }
            array_indexed_records.push((
                sidecar,
                crate::array::ArrayIndexedSnapshot {
                    length_writable,
                    property_flags,
                },
            ));
        }
        let next_shape_id = input.u64()?;
        let realm_context_base = Some(input.u32()?).filter(|&base| base != u32::MAX);
        Some(Self {
            image,
            code_space,
            dynamic_natives,
            fixed_roots,
            iterator_prototype_roots,
            function_kind_prototype_roots,
            external_ref_addrs,
            atom_names,
            global_lexicals,
            registered_symbols,
            regexp_payloads,
            array_indexed_records,
            next_shape_id,
            realm_context_base,
            image_slide,
        })
    }
}
