//! Runtime variable-environment record for direct `eval`.
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
//!
//! # Invariants
//! - Created at frame entry for any function whose compiled record
//!   has `contains_direct_eval`; closures made inside capture the
//!   handle, so the chain mirrors the lexical function nesting.
//! - Cells are append-only; `names[i]` labels `cells[i]`.
//!
//! # See also
//! - `global_ops` (the dynamic Load/Store/Typeof walkers)
//! - `eval_ops` (binding adoption from a compiled eval body)

use otter_macros::Pelt;

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
    /// The enclosing function's record, when that function also
    /// contains a direct eval call site.
    pub parent: Option<otter_gc::Gc<EvalEnvBody>>,
}

/// 4-byte compressed GC handle.
pub type EvalEnvHandle = otter_gc::Gc<EvalEnvBody>;

/// Allocate a fresh, empty record.
pub fn alloc_eval_env(
    heap: &mut otter_gc::GcHeap,
    parent: Option<EvalEnvHandle>,
) -> Result<EvalEnvHandle, otter_gc::OutOfMemory> {
    heap.alloc_old(EvalEnvBody {
        names: Vec::new(),
        cells: Vec::new(),
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

/// Remove `name` from the nearest env in the chain that binds it —
/// §19.2.1.3 eval-created var bindings are CreateMutableBinding(vn,
/// true), i.e. deletable. Lookup is by name on every read, so index
/// compaction here cannot invalidate anything.
pub fn eval_env_delete(heap: &mut otter_gc::GcHeap, env: EvalEnvHandle, name: &str) -> bool {
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

/// Find `name` in `env` or any ancestor record.
#[must_use]
pub fn eval_env_lookup(
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
                .position(|n| n == name)
                .map(|i| body.cells[i]);
            (found, body.parent)
        });
        if found.is_some() {
            return found;
        }
        current = parent;
    }
    None
}

/// Append a binding (the caller guarantees the name is fresh in
/// THIS record; shadowing across records is resolved by lookup
/// order).
pub fn eval_env_insert(
    heap: &mut otter_gc::GcHeap,
    env: EvalEnvHandle,
    name: String,
    cell: UpvalueCell,
) {
    heap.with_payload(env, |body| {
        body.names.push(name);
        body.cells.push(cell);
    });
    heap.record_write(env, &cell);
}
