//! Census of the native callables a built runtime leaves on the heap.
//!
//! An opaque in-process snapshot copies [`NativeFunctionBody`] pages verbatim.
//! Static entry points remain valid in the same process; dynamic-native `Arc`s
//! live in the host-ref table and are cloned at their exact indices. The
//! display name is an ordinary heap-owned string.
//!
//! This module counts them honestly — by walking live bodies, not by grepping
//! declaration sites — so diagnostics expose dispatch mix, capture pressure,
//! and host-ref ownership without rendering process addresses.
//!
//! # Contents
//!
//! - [`NativeStorageKind`] — which dispatch payload a body holds.
//! - [`NativeBodyFacts`] — one body's non-GC payload.
//! - [`DynamicNativeRow`] — a closure-backed native and how many
//!   copies of it are live.
//! - [`NativeCensus`] — totals, plus the enumerable closure list.
//! - [`native_census`] — build one from a heap.
//!
//! # Invariants
//!
//! - Every live [`NativeFunctionBody`] lands in exactly one storage
//!   bucket: `static_count + vm_intrinsic_count + dynamic_count +
//!   local_dynamic_count == total`.
//! - `distinct_static_fns` counts unique function addresses, so it is
//!   the lower bound on a native reference table's size, while
//!   `static_count` is the number of bodies using those identities.
//! - [`NativeCensus::closures`] is sorted by name so the rendered
//!   table is stable across runs; addresses are never rendered.
//!
//! # See also
//!
//! - [`crate::native_function`] — the bodies being counted.
//! - [`otter_gc::census`] — the per-space heap census this rides on.

use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;

use otter_gc::GcHeap;
use otter_gc::page::SpaceKind;

use crate::native_function::NativeFunctionBody;

/// Which dispatch payload a [`NativeFunctionBody`] holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum NativeStorageKind {
    /// Plain `fn` pointer — resolvable through a build-time table.
    Static,
    /// VM-owned intrinsic dispatched by the interpreter; a plain enum
    /// discriminant, so it survives a dump unchanged.
    VmIntrinsic,
    /// `Arc<dyn Fn + Send + Sync>` closure — not serializable.
    Dynamic,
    /// `Arc<dyn Fn>` isolate-local closure — not serializable.
    LocalDynamic,
}

impl NativeStorageKind {
    /// `true` when a dumped page cannot carry this payload and the
    /// restore path has to re-install the callable by name.
    #[must_use]
    pub fn needs_reinstall(self) -> bool {
        matches!(self, Self::Dynamic | Self::LocalDynamic)
    }
}

/// One body's non-GC payload, as reported by
/// `NativeFunctionBody::census_facts`.
#[derive(Debug, Clone, Copy)]
pub struct NativeBodyFacts {
    /// Display name — the body's heap-owned string.
    pub name: crate::string::JsString,
    /// Storage bucket.
    pub kind: NativeStorageKind,
    /// Raw entry address for [`NativeStorageKind::Static`] bodies.
    pub static_addr: Option<usize>,
    /// External-reference index identifying the static entry, or
    /// [`otter_gc::NO_EXTERNAL_REF`] when the body is not static-backed.
    pub native_ref: u32,
    /// Traced JS values the payload owns.
    pub capture_count: usize,
}

/// One closure-backed native and how many live copies exist.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DynamicNativeRow {
    /// Display name the restore path would re-install under.
    pub name: String,
    /// Which closure storage the body uses.
    pub kind: NativeStorageKind,
    /// Live bodies sharing this name and kind.
    pub count: u64,
}

/// Totals over every live [`NativeFunctionBody`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeCensus {
    /// Live native-function bodies.
    pub total: u64,
    /// Bodies in old space — the set a bootstrap snapshot dumps.
    pub in_old_space: u64,
    /// Bodies outside old space, which a bootstrap snapshot would
    /// miss. Nonzero here means `set_tenure_all` did not cover the
    /// whole build.
    pub outside_old_space: u64,
    /// Bodies holding a plain `fn` pointer.
    pub static_count: u64,
    /// Distinct static entry addresses behind `static_count`.
    pub distinct_static_fns: u64,
    /// Bodies dispatched as a VM intrinsic.
    pub vm_intrinsic_count: u64,
    /// Bodies holding an `Arc<NativeFn>`.
    pub dynamic_count: u64,
    /// Bodies holding an `Arc<LocalNativeFn>`.
    pub local_dynamic_count: u64,
    /// Bodies carrying an external-reference index.
    pub native_ref_count: u64,
    /// Bodies whose index resolves to the entry address their storage
    /// actually holds. Must equal [`Self::static_count`]: an index that
    /// does not round-trip would restore to the wrong function.
    pub resolved_native_ref_count: u64,
    /// Entries in the isolate's external-reference table.
    pub external_ref_table_len: u64,
    /// Bodies owning at least one traced JS capture.
    pub with_captures_count: u64,
    /// Total captured values across every body.
    pub capture_total: u64,
    /// Every closure-backed native, sorted by `(name, kind)`. This is
    /// the list a restore path re-installs by name.
    pub closures: Vec<DynamicNativeRow>,
}

impl NativeCensus {
    /// Render as a deterministic text block.
    #[must_use]
    pub fn render_text(&self) -> String {
        let mut out = String::with_capacity(1024);
        let _ = writeln!(
            out,
            "; native callables — total={} old_space={} outside_old_space={}",
            self.total, self.in_old_space, self.outside_old_space,
        );
        let _ = writeln!(
            out,
            "  static={} (distinct fns {}), vm_intrinsic={}, dynamic={}, local_dynamic={}",
            self.static_count,
            self.distinct_static_fns,
            self.vm_intrinsic_count,
            self.dynamic_count,
            self.local_dynamic_count,
        );
        let _ = writeln!(
            out,
            "  external refs={}/{} resolved (table {} entries), \
             bodies with captures={} (values {})",
            self.resolved_native_ref_count,
            self.native_ref_count,
            self.external_ref_table_len,
            self.with_captures_count,
            self.capture_total,
        );
        if self.closures.is_empty() {
            let _ = writeln!(out, "  no closure-backed natives");
            return out;
        }
        let _ = writeln!(out, "  closure-backed natives to re-install by name:");
        for row in &self.closures {
            let _ = writeln!(out, "    {:>5}x  {:?}  {}", row.count, row.kind, row.name);
        }
        out
    }
}

/// Walk every live [`NativeFunctionBody`] on `heap` and tally its
/// dispatch storage.
///
/// Runs under the same single-mutator contract as
/// [`otter_gc::GcHeap::census`]: `&heap` while no allocator path is
/// open.
#[must_use]
pub fn native_census(heap: &GcHeap) -> NativeCensus {
    let mut census = NativeCensus {
        total: 0,
        in_old_space: 0,
        outside_old_space: 0,
        static_count: 0,
        distinct_static_fns: 0,
        vm_intrinsic_count: 0,
        dynamic_count: 0,
        local_dynamic_count: 0,
        native_ref_count: 0,
        resolved_native_ref_count: 0,
        external_ref_table_len: heap.external_refs().len() as u64,
        with_captures_count: 0,
        capture_total: 0,
        closures: Vec::new(),
    };
    let mut static_addrs: HashSet<usize> = HashSet::new();
    let mut closures: BTreeMap<(String, NativeStorageKind), u64> = BTreeMap::new();

    heap.for_each_live_payload::<NativeFunctionBody, _>(|space, body| {
        let facts = body.census_facts();
        census.total += 1;
        if space == SpaceKind::Old {
            census.in_old_space += 1;
        } else {
            census.outside_old_space += 1;
        }
        match facts.kind {
            NativeStorageKind::Static => {
                census.static_count += 1;
                if let Some(addr) = facts.static_addr {
                    static_addrs.insert(addr);
                }
            }
            NativeStorageKind::VmIntrinsic => census.vm_intrinsic_count += 1,
            NativeStorageKind::Dynamic => census.dynamic_count += 1,
            NativeStorageKind::LocalDynamic => census.local_dynamic_count += 1,
        }
        if facts.kind.needs_reinstall() {
            *closures
                .entry((facts.name.to_lossy_string(heap), facts.kind))
                .or_default() += 1;
        }
        if facts.native_ref != otter_gc::NO_EXTERNAL_REF {
            census.native_ref_count += 1;
            if heap.external_refs().address(facts.native_ref) == facts.static_addr {
                census.resolved_native_ref_count += 1;
            }
        }
        if facts.capture_count != 0 {
            census.with_captures_count += 1;
        }
        census.capture_total += facts.capture_count as u64;
    });

    census.distinct_static_fns = static_addrs.len() as u64;
    census.closures = closures
        .into_iter()
        .map(|((name, kind), count)| DynamicNativeRow { name, kind, count })
        .collect();
    census
}
