//! Store programs for receivers whose exact shape compiled code proves.
//!
//! A megamorphic named store site carries no per-receiver programs: its
//! receivers are many, and the shared action table serves them. A receiver
//! compiled code knows exactly — the `this` its own construct entry
//! allocates, or one an inlined construct allocates — needs none of that:
//! what a store does to an object of one exact shape follows from the shape,
//! its interned transitions and the live prototype chain, as a monomorphic
//! site's program would record it.
//!
//! # Contents
//! - `bake_own_receiver_plan` names the receiver the body's own construct
//!   entry allocates, when one steady family fixes it.
//! - `bake_proved_stores_tree` collects the receiver shapes the compiled tree
//!   can prove (construct roots and their interned descendants) and bakes,
//!   for every megamorphic store site of every body, one program per shape.
//!
//! # Invariants
//! - A program is exactly the one a monomorphic site would hold for that
//!   receiver shape: shape guard, chain proof (null link or validity cell),
//!   extensibility and geometry guards, then the store and the published
//!   child. Every shape and validity cell it names enters the compile roots.
//!   As for exported programs, a prototype-role shape, or a shape that
//!   cannot grow for an append, gets none.
//! - Programs are keyed by `(byte PC, shape)` beside the site's own: a tier
//!   uses one only for a receiver proved to have that shape.
//! - Nothing here allocates on the GC heap or changes any shape or table.
//!
//! # See also
//! - [`crate::object::shape_store`] resolves one store.
//! - `crate::property_ic` builds the same programs from installed handlers.

use super::Baker;
use crate::constructor_layout::ConstructorFamilyOwner;
use crate::jit::{JitCacheIrOp, JitCacheIrProgram, JitCompileSnapshot};
use crate::object::{
    ShapeHandle, ShapeStore, StorePropertyTransitionKind, shape_body,
    shape_interner::ShapeInterner, shape_store,
};
use crate::property_atom::{AtomId, AtomizedPropertyKey, PropertyAtom};
use otter_bytecode::Op;

/// Shapes one compiled tree may prove and bake store programs for.
const MAX_PROVED_SHAPES: usize = 64;

impl Baker<'_> {
    /// The receiver `fid`'s own construct entry allocates: the plan of its
    /// one finalized, attached base-constructor family whose new.target is a
    /// closure of `fid` itself, fitted with the family's empty root.
    pub(super) fn bake_own_receiver_plan(&mut self, view: &mut JitCompileSnapshot, fid: u32) {
        if view.derived_constructor {
            return;
        }
        let heap = &self.vm.gc_heap;
        let mut family = None;
        for layout in self.vm.constructor_families.of_owner(fid) {
            let admitted = heap.read_payload(layout, |body| {
                (body.owner() == (ConstructorFamilyOwner::Closure, fid)
                    && body.base_function_id() == fid
                    && body.finalized()
                    && !body.detached())
                .then(|| body.family_id())
            });
            match (admitted, family) {
                (None, _) => {}
                (Some(id), None) => family = Some(id),
                // Two families allocate differently: no single entry shape.
                (Some(_), Some(_)) => return,
            }
        }
        let Some((mut plan, _)) =
            family.and_then(|family| self.bake_receiver_allocation_plan(fid, family))
        else {
            return;
        };
        if plan.new_target_is_class {
            return;
        }
        // The entry's other receivers are the root's too: fit the root alone.
        plan.receiver_shape = 0;
        plan.initial_field_count = 0;
        view.own_receiver_plan = Some(plan);
    }

    /// Bake [`JitCompileSnapshot::proved_store_programs`] across the whole
    /// compiled tree rooted at `view`.
    pub(super) fn bake_proved_stores_tree(&mut self, view: &mut JitCompileSnapshot) {
        let mut roots = Vec::new();
        collect_roots(view, &mut roots);
        let shapes = self.descendant_shapes(&roots);
        if shapes.is_empty() {
            return;
        }
        self.bake_proved_stores(view, &shapes);
    }

    fn bake_proved_stores(&mut self, view: &mut JitCompileSnapshot, shapes: &[ShapeHandle]) {
        let names = crate::isolate_names::names(&self.vm.gc_heap);
        let sites: Vec<(u32, u32)> = view
            .instructions
            .iter()
            .filter(|instruction| instruction.op(&view.code_block) == Op::StoreProperty)
            .filter_map(|instruction| {
                let access = view.property_accesses.get(&instruction.byte_pc)?;
                access.shared.then_some((instruction.byte_pc, access.atom))
            })
            .collect();
        for (byte_pc, atom) in sites {
            let atom = AtomId::from_global(atom);
            let Some(name) = names.spelling(atom) else {
                continue;
            };
            let key = AtomizedPropertyKey::new(PropertyAtom::new(atom), name);
            for &shape in shapes {
                if let Some((token, program)) = self.proved_store_program(shape, key) {
                    view.proved_store_programs.insert((byte_pc, token), program);
                }
            }
        }
        for callee in view.inline_callees.values_mut() {
            self.bake_proved_stores(std::sync::Arc::make_mut(&mut callee.body), shapes);
        }
        for methods in view.method_targets.values_mut() {
            for method in methods {
                if let Some(body) = &mut method.body {
                    self.bake_proved_stores(std::sync::Arc::make_mut(body), shapes);
                }
            }
        }
    }

    /// The program of a store of `key` into an object of exactly `shape`.
    fn proved_store_program(
        &mut self,
        shape: ShapeHandle,
        key: AtomizedPropertyKey<'_>,
    ) -> Option<(u32, JitCacheIrProgram)> {
        let store = shape_store(&self.vm.gc_heap, shape, key)?;
        // As an exported program: never into a prototype, never an append to
        // an object that cannot grow.
        let state = crate::object::shape_body::state_of(shape);
        if state.is_prototype()
            || (matches!(store, ShapeStore::Append { .. }) && !state.is_extensible())
        {
            return None;
        }
        let token = self.bake_shape(shape)?;
        let atom = key.atom().id().raw();
        let ops = match store {
            ShapeStore::Own(field) => vec![
                JitCacheIrOp::GuardShape {
                    object: 0,
                    shape: token,
                },
                JitCacheIrOp::GuardAtomSlot {
                    object: 0,
                    atom,
                    field,
                    writable: true,
                },
                JitCacheIrOp::StoreField { object: 0, field },
            ],
            ShapeStore::Append { child, field, kind } => {
                let child = self.bake_shape(child)?;
                let chain = match kind {
                    StorePropertyTransitionKind::OwnAdd => {
                        JitCacheIrOp::GuardPrototypeNull { object: 0 }
                    }
                    StorePropertyTransitionKind::PrototypeChainMissing { validity }
                    | StorePropertyTransitionKind::PrototypeWritableData { validity } => {
                        JitCacheIrOp::GuardPrototypeValidity {
                            validity: self.bake_prototype_validity(&validity)?,
                        }
                    }
                };
                vec![
                    JitCacheIrOp::GuardShape {
                        object: 0,
                        shape: token,
                    },
                    chain,
                    JitCacheIrOp::GuardExtensible { object: 0, field },
                    JitCacheIrOp::StoreField { object: 0, field },
                    JitCacheIrOp::PublishShape {
                        object: 0,
                        shape: child,
                    },
                ]
            }
        };
        Some((
            token,
            JitCacheIrProgram {
                ops: ops.into_boxed_slice(),
            },
        ))
    }

    /// `roots` and their interned descendants, breadth first, at most
    /// [`MAX_PROVED_SHAPES`] of them.
    fn descendant_shapes(&mut self, roots: &[u32]) -> Vec<ShapeHandle> {
        let mut shapes: Vec<ShapeHandle> = Vec::new();
        for &token in roots {
            // SAFETY: every root token is a shape this session's baker
            // rooted for the compilation.
            let shape = unsafe { ShapeHandle::from_offset(token) };
            if !shape.is_null() && !shapes.contains(&shape) {
                shapes.push(shape);
            }
        }
        let mut next = 0;
        while next < shapes.len() && shapes.len() < MAX_PROVED_SHAPES {
            for child in ShapeInterner::children_of(&self.vm.gc_heap, shapes[next]) {
                if shapes.len() == MAX_PROVED_SHAPES {
                    break;
                }
                if !shapes.contains(&child) {
                    shapes.push(child);
                }
            }
            next += 1;
        }
        shapes
    }
}

/// Receiver shapes the tree may prove: its own construct entry's root and
/// every receiver an allocation plan of a construct site in it creates.
fn collect_roots(view: &JitCompileSnapshot, roots: &mut Vec<u32>) {
    if let Some(plan) = view.own_receiver_plan {
        roots.push(plan.prototype_root);
    }
    for callee in view.direct_constructs.values() {
        if let Some(plan) = callee.receiver_allocation {
            roots.extend([plan.prototype_root, plan.receiver_shape]);
        }
    }
    for callee in view.inline_callees.values() {
        collect_roots(&callee.body, roots);
    }
    roots.retain(|&token| token != 0);
}
