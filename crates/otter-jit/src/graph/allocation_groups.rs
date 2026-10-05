//! Fixed allocation groups over consecutive original source nodes.
//!
//! # Contents
//! - Bounded exact-realm empty object/array shell recipes.
//! - Post-LICM, pre-register-allocation replacement and tagged projections.
//!
//! # Invariants
//! Only 2..8 consecutive fixed shells in one block and source body are folded.
//! The exact source view must have admitted machine allocation at preparation;
//! a disabled or unprepared snapshot retains the standalone native nodes.
//! Calls, guards, other operations and source changes end a group. The first
//! original node owns the single collecting admission boundary and eager state;
//! later original ids project initialized tagged cells without a safepoint.
//! No interior Word address crosses a boundary. Dynamic strings, CopyContext,
//! nonempty literals and lexical allocations remain separate source operations.
//!
//! # See also
//! - `crate::allocation::EmptyLiteralLayout` owns each VM geometry recipe.
//! - The target LAB owner initializes every cell before one top publication.

use super::ir::{BlockId, Graph, Kind, NodeId, Repr};
use crate::allocation::EmptyLiteralLayout;
use otter_vm::JitCompileSnapshot;

#[derive(Debug, Clone)]
pub(crate) struct Member {
    pub(crate) node: NodeId,
    pub(crate) byte: u32,
    pub(crate) layout: EmptyLiteralLayout,
}
#[derive(Debug, Clone)]
pub(crate) struct Group {
    pub(crate) bytes: u32,
    pub(crate) realm: u32,
    pub(crate) members: Vec<Member>,
}
fn recipe(view: &JitCompileSnapshot, graph: &Graph, id: NodeId) -> Option<EmptyLiteralLayout> {
    if !view.literal_allocations.group_allowed {
        return None;
    }
    let node = graph.node(id);
    if node.repr != Repr::Tagged || !node.inputs.is_empty() || node.eager.is_none() {
        return None;
    }
    match node.kind {
        Kind::NewObject => view
            .literal_allocations
            .object
            .map(EmptyLiteralLayout::Object),
        Kind::NewArrayEmpty if view.literal_allocations.realm_id == 0 => {
            Some(EmptyLiteralLayout::Array(view.literal_allocations.array))
        }
        _ => None,
    }
}

pub(crate) fn fold(
    graph: &mut Graph,
    layout: &[BlockId],
    root: &JitCompileSnapshot,
    inlines: &[std::sync::Arc<JitCompileSnapshot>],
) {
    for &block in layout {
        let body = graph.block(block).body.clone();
        let mut position = 0;
        while position < body.len() {
            let first = body[position];
            let origin = graph.node(first).origin;
            let Some(view) = (if origin == 0 {
                Some(root)
            } else {
                inlines.get(origin as usize - 1).map(AsRef::as_ref)
            }) else {
                position += 1;
                continue;
            };
            let mut members = Vec::new();
            let mut bytes = 0u32;
            for &id in body[position..].iter().take(8) {
                if graph.node(id).origin != origin {
                    break;
                }
                let Some(recipe) = recipe(view, graph, id) else {
                    break;
                };
                let Some(end) = bytes.checked_add(recipe.bytes()) else {
                    break;
                };
                // Every member offset is an unsigned 12-bit immediate on
                // AArch64. This target-neutral bound also keeps the small fixed
                // group below one nursery page without a JIT -> GC dependency.
                if end > 4095 {
                    break;
                }
                members.push(Member {
                    node: id,
                    byte: bytes,
                    layout: recipe,
                });
                bytes = end;
            }
            if members.len() < 2 {
                position += 1;
                continue;
            }
            let index = graph.allocation_groups.len() as u32;
            graph.node_mut(first).kind = Kind::AllocationGroup(index);
            for member in &members[1..] {
                let node = graph.node_mut(member.node);
                node.kind = Kind::AllocationProjection(member.byte);
                node.inputs = smallvec::smallvec![first];
                node.eager = None;
                node.lazy = None;
            }
            position += members.len();
            graph.allocation_groups.push(Group {
                bytes,
                realm: view.literal_allocations.realm_id,
                members,
            });
        }
    }
}
