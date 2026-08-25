//! Runtime variable-environment records for direct `eval`.
//!
//! §9.1 — a direct `eval` in sloppy code declares its `var` bindings
//! in the CALLER's variable environment. Those bindings must be
//! visible to every closure whose scope chain contains that
//! environment — including closures created BEFORE the eval ran
//! (e.g. a parameter-default function observing a var the next
//! parameter's eval introduces) and ones that outlive the frame.
//!
//! # Contents
//! - [`EvalEnvBody`] — GC-owned name → cell table with a parent link.
//! - [`EvalEnvHandle`] — 4-byte GC handle.
//! - [`EvalEnvSnapshotBinding`] — owned compiler-facing chain snapshot.
//!
//! # Invariants
//! - Created at frame entry for any function whose compiled record has
//!   `contains_direct_eval`; closures made inside capture the handle, so the
//!   chain mirrors the lexical function nesting.
//! - `names[i]` always labels `cells[i]`; deletion compacts both vectors in
//!   lockstep and no compiled code retains a positional index.
//! - The nearest record wins lookup and deletion. Snapshotting removes shadowed
//!   ancestors and sorts the remaining owned names deterministically.
//! - The current record is the only insertion target. Sloppy direct eval reuses
//!   its caller's current record; strict direct eval owns a fresh child record.
//!
//! # See also
//! - `global_ops` (the dynamic Load/Store/Typeof walkers)
//! - `eval_ops` (binding adoption from a compiled eval body)

use otter_macros::Pelt;
use std::collections::{BTreeMap, HashSet};

use crate::UpvalueCell;

/// Reserved [`otter_gc::Traceable::TYPE_TAG`] for [`EvalEnvBody`].
pub const EVAL_ENV_BODY_TYPE_TAG: u8 = 0x2E;

/// GC body for one function-scope eval environment record.
#[derive(Debug, Pelt)]
#[pelt(tag = EVAL_ENV_BODY_TYPE_TAG)]
pub struct EvalEnvBody {
    /// Binding names, parallel to `cells`. Plain Rust strings — not
    /// GC slots.
    #[pelt(skip)]
    pub names: Vec<String>,
    /// One live cell per eval-introduced binding.
    pub cells: Vec<UpvalueCell>,
    /// Isolate-issued creation sequence, parallel to `cells`. Snapshot-
    /// filtered lookups (§13.15.2 pre-RHS reference resolution) skip
    /// bindings whose sequence is at or past the snapshot.
    #[pelt(skip)]
    pub seqs: Vec<u64>,
    /// The enclosing function's record, when that function also
    /// contains a direct eval call site.
    pub parent: Option<otter_gc::Gc<EvalEnvBody>>,
}

/// 4-byte compressed GC handle.
pub type EvalEnvHandle = otter_gc::Gc<EvalEnvBody>;

/// One unique binding in an owned snapshot of an eval-environment chain.
///
/// Snapshots are sorted by `name` for deterministic compiler slot layout. When
/// more than one record binds the same name, only the nearest binding appears.
#[derive(Debug, Clone)]
pub(crate) struct EvalEnvSnapshotBinding {
    /// Owned source-level binding name.
    pub(crate) name: String,
    /// Zero for the current record, increasing toward outer ancestors.
    pub(crate) depth: usize,
    /// Whether this binding belongs to the current record.
    pub(crate) current: bool,
}

/// Allocate a fresh, empty record.
#[cfg(test)]
pub(crate) fn alloc_eval_env(
    heap: &mut otter_gc::GcHeap,
    parent: Option<EvalEnvHandle>,
) -> Result<EvalEnvHandle, otter_gc::OutOfMemory> {
    heap.alloc_old(EvalEnvBody {
        names: Vec::new(),
        cells: Vec::new(),
        seqs: Vec::new(),
        parent,
    })
}

/// Allocate a fresh record while tracing a not-yet-published frame and the
/// parent slot. The body is first allocated with no parent, then linked to the
/// relocated parent after allocation; this avoids copying a pre-GC handle into
/// the untraced allocation payload.
pub(crate) fn alloc_eval_env_with_roots(
    heap: &mut otter_gc::GcHeap,
    mut parent: Option<EvalEnvHandle>,
    external_visit: &mut otter_gc::heap::RootSlotVisitor<'_>,
) -> Result<EvalEnvHandle, otter_gc::OutOfMemory> {
    let mut visit = |visitor: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {
        external_visit(visitor);
        if let Some(parent) = &mut parent {
            visitor(parent as *mut EvalEnvHandle as *mut otter_gc::raw::RawGc);
        }
    };
    let env = heap.alloc_old_with_roots(
        EvalEnvBody {
            names: Vec::new(),
            cells: Vec::new(),
            seqs: Vec::new(),
            parent: None,
        },
        &mut visit,
    )?;
    if let Some(parent) = parent {
        heap.with_payload(env, |body| body.parent = Some(parent));
        heap.write_barrier(env, parent);
    }
    Ok(env)
}

/// Find `name` in exactly the current record.
#[must_use]
#[cfg(test)]
pub(crate) fn eval_env_lookup_current(
    heap: &otter_gc::GcHeap,
    env: EvalEnvHandle,
    name: &str,
) -> Option<UpvalueCell> {
    heap.read_payload(env, |body| {
        body.names
            .iter()
            .position(|candidate| candidate == name)
            .map(|index| body.cells[index])
    })
}

/// Find `name` in the current record or its nearest binding ancestor.
#[must_use]
pub fn eval_env_lookup_chain(
    heap: &otter_gc::GcHeap,
    env: EvalEnvHandle,
    name: &str,
) -> Option<UpvalueCell> {
    let mut current = Some(env);
    while let Some(handle) = current {
        let (found, parent) = heap.read_payload(handle, |body| {
            let found = body
                .names
                .iter()
                .position(|candidate| candidate == name)
                .map(|index| body.cells[index]);
            (found, body.parent)
        });
        if found.is_some() {
            return found;
        }
        current = parent;
    }
    None
}

/// Find `name` within at most `depth` physical records starting at `env`.
///
/// The shadowed-upvalue opcodes carry a compiler-computed bound: only eval
/// records strictly inside the captured binding's declaration owner may
/// shadow it, so the probe must not walk past them into outer scopes. With a
/// `snapshot`, only bindings created before it are visible — the environment
/// exactly as a reference resolved at snapshot time saw it (§13.15.2 PutValue
/// through a pre-RHS reference).
pub fn eval_env_lookup_chain_bounded_snap(
    heap: &otter_gc::GcHeap,
    env: EvalEnvHandle,
    name: &str,
    depth: u32,
    snapshot: Option<u64>,
) -> Option<UpvalueCell> {
    let mut current = Some(env);
    let mut remaining = depth;
    while let Some(handle) = current {
        if remaining == 0 {
            return None;
        }
        remaining -= 1;
        let (found, parent) = heap.read_payload(handle, |body| {
            let found = body
                .names
                .iter()
                .position(|candidate| candidate == name)
                .filter(|&index| {
                    snapshot.is_none_or(|snapshot| {
                        body.seqs.get(index).copied().unwrap_or(0) < snapshot
                    })
                })
                .map(|index| body.cells[index]);
            (found, body.parent)
        });
        if found.is_some() {
            return found;
        }
        current = parent;
    }
    None
}

/// Remove `name` from the nearest record within the bounded prefix.
///
/// Mirrors [`eval_env_delete_chain`] with the same compiler-computed bound as
/// [`eval_env_lookup_chain_bounded_snap`]: a binding declared outside the prefix is
/// left intact and the delete reports `false`.
pub fn eval_env_delete_chain_bounded(
    heap: &mut otter_gc::GcHeap,
    env: EvalEnvHandle,
    name: &str,
    depth: u32,
) -> bool {
    let mut current = Some(env);
    let mut remaining = depth;
    while let Some(handle) = current {
        if remaining == 0 {
            return false;
        }
        remaining -= 1;
        let (removed, parent) = heap.with_payload(handle, |body| {
            match body.names.iter().position(|n| n == name) {
                Some(i) => {
                    body.names.remove(i);
                    body.cells.remove(i);
                    (true, None)
                }
                None => (false, body.parent),
            }
        });
        if removed {
            return true;
        }
        current = parent;
    }
    false
}

/// Insert a fresh binding into exactly the current record.
///
/// Returns `false` without modifying the record when `name` already belongs to
/// that record. Shadowing an ancestor remains valid and returns `true`.
pub fn eval_env_insert_current(
    heap: &mut otter_gc::GcHeap,
    env: EvalEnvHandle,
    name: String,
    cell: UpvalueCell,
    seq: u64,
) -> bool {
    let inserted = heap.with_payload(env, |body| {
        if body.names.iter().any(|candidate| candidate == &name) {
            return false;
        }
        body.names.push(name);
        body.cells.push(cell);
        body.seqs.push(seq);
        true
    });
    if inserted {
        heap.record_write(env, &cell);
    }
    inserted
}

/// Remove `name` from the nearest record in the chain that binds it.
///
/// Eval-created `var` bindings are deletable (§19.2.1.3
/// CreateMutableBinding(vn, true)). Lookup remains name-based, so compacting
/// the parallel vectors cannot invalidate compiled slot metadata.
pub fn eval_env_delete_chain(heap: &mut otter_gc::GcHeap, env: EvalEnvHandle, name: &str) -> bool {
    let mut current = Some(env);
    while let Some(handle) = current {
        let (removed, parent) = heap.with_payload(handle, |body| {
            match body.names.iter().position(|n| n == name) {
                Some(i) => {
                    body.names.remove(i);
                    body.cells.remove(i);
                    (true, None)
                }
                None => (false, body.parent),
            }
        });
        if removed {
            return true;
        }
        current = parent;
    }
    false
}

/// Snapshot every unique binding in `env` and its ancestors.
///
/// Traversal is nearest-first, so a shadowed ancestor is omitted. The returned
/// vector is sorted by name rather than by hash or allocation order, providing
/// one deterministic slot order to the eval compiler.
#[must_use]
pub(crate) fn eval_env_snapshot_chain(
    heap: &otter_gc::GcHeap,
    env: EvalEnvHandle,
) -> Vec<EvalEnvSnapshotBinding> {
    let mut seen = HashSet::new();
    let mut bindings = BTreeMap::new();
    let mut current = Some(env);
    let mut depth = 0usize;
    while let Some(handle) = current {
        let parent = heap.read_payload(handle, |body| {
            debug_assert_eq!(body.names.len(), body.cells.len());
            for name in &body.names {
                if seen.insert(name.clone()) {
                    bindings.insert(
                        name.clone(),
                        EvalEnvSnapshotBinding {
                            name: name.clone(),
                            depth,
                            current: depth == 0,
                        },
                    );
                }
            }
            body.parent
        });
        current = parent;
        depth += 1;
    }
    bindings.into_values().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Value;

    fn cell(heap: &mut otter_gc::GcHeap, value: i32) -> UpvalueCell {
        crate::alloc_upvalue(heap, Value::number_i32(value)).expect("upvalue cell")
    }

    #[test]
    fn chain_snapshot_is_unique_nearest_first_and_name_sorted() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let outer_a = cell(&mut heap, 1);
        let outer_dup = cell(&mut heap, 2);
        let outer_z = cell(&mut heap, 3);
        let inner_b = cell(&mut heap, 4);
        let inner_dup = cell(&mut heap, 5);

        let outer = alloc_eval_env(&mut heap, None).expect("outer env");
        assert!(eval_env_insert_current(
            &mut heap,
            outer,
            "z".to_string(),
            outer_z,
            1,
        ));
        assert!(eval_env_insert_current(
            &mut heap,
            outer,
            "dup".to_string(),
            outer_dup,
            2,
        ));
        assert!(eval_env_insert_current(
            &mut heap,
            outer,
            "a".to_string(),
            outer_a,
            3,
        ));

        let mut no_extra_roots = |_: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {};
        let inner = alloc_eval_env_with_roots(&mut heap, Some(outer), &mut no_extra_roots)
            .expect("inner env");
        assert!(eval_env_insert_current(
            &mut heap,
            inner,
            "b".to_string(),
            inner_b,
            4,
        ));
        assert!(eval_env_insert_current(
            &mut heap,
            inner,
            "dup".to_string(),
            inner_dup,
            5,
        ));
        assert!(!eval_env_insert_current(
            &mut heap,
            inner,
            "dup".to_string(),
            outer_dup,
            6,
        ));

        assert_eq!(eval_env_lookup_current(&heap, inner, "a"), None);
        assert_eq!(eval_env_lookup_chain(&heap, inner, "a"), Some(outer_a));
        assert_eq!(eval_env_lookup_chain(&heap, inner, "dup"), Some(inner_dup));

        let snapshot = eval_env_snapshot_chain(&heap, inner);
        assert_eq!(
            snapshot
                .iter()
                .map(|binding| binding.name.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b", "dup", "z"],
        );
        let a = snapshot
            .iter()
            .find(|binding| binding.name == "a")
            .expect("outer binding");
        assert_eq!(a.depth, 1);
        assert!(!a.current);
        let duplicate = snapshot
            .iter()
            .find(|binding| binding.name == "dup")
            .expect("nearest duplicate");
        assert_eq!(duplicate.depth, 0);
        assert!(duplicate.current);
    }

    #[test]
    fn deletion_removes_only_the_nearest_matching_record() {
        let mut heap = otter_gc::GcHeap::new().expect("gc heap");
        let outer_cell = cell(&mut heap, 1);
        let inner_cell = cell(&mut heap, 2);
        let outer = alloc_eval_env(&mut heap, None).expect("outer env");
        assert!(eval_env_insert_current(
            &mut heap,
            outer,
            "x".to_string(),
            outer_cell,
            7,
        ));
        let mut no_extra_roots = |_: &mut dyn FnMut(*mut otter_gc::raw::RawGc)| {};
        let inner = alloc_eval_env_with_roots(&mut heap, Some(outer), &mut no_extra_roots)
            .expect("inner env");
        assert!(eval_env_insert_current(
            &mut heap,
            inner,
            "x".to_string(),
            inner_cell,
            8,
        ));

        assert!(eval_env_delete_chain(&mut heap, inner, "x"));
        assert_eq!(eval_env_lookup_current(&heap, inner, "x"), None);
        assert_eq!(eval_env_lookup_chain(&heap, inner, "x"), Some(outer_cell));
        assert!(eval_env_delete_chain(&mut heap, inner, "x"));
        assert_eq!(eval_env_lookup_chain(&heap, inner, "x"), None);
        assert!(!eval_env_delete_chain(&mut heap, inner, "x"));
    }
}
