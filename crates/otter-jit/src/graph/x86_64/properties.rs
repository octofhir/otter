//! x86-64 Graph named properties, shared caches and ordinary prototype walks.
//!
//! # Contents
//! - Selected source CacheIR load programs, stores and guarded method holders.
//! - One authoritative property-action table with live logical-slot banks.
//! - Source-owned committed property calls and default instanceof walks.
//!
//! # Invariants
//! - Every store guard completes before the first slot or shape write.
//! - Shape transitions clear the source shape's stability byte before
//!   publishing the child shape.
//! - Defining field owners observe the complete store and child publication;
//!   membership misses use a noncollecting leaf with live-register preservation.
//! - Static stores use their source snapshot's captured membership only for
//!   the defining owner selected by the destination shape and logical field.
//! - Selected programs keep ordinary descriptor, prototype and capacity guards
//!   before effects. A numeric RHS is encoded into a separate GP word; all
//!   property reads become canonical before they enter Tagged state.
//! - Cold operations run once with canonical roots, never by replaying a hit.
//! - Instanceof passes its operation explicitly while source records retain
//!   root and reconstruction ownership.
//! - Mutable fields are read live; shape and chain identities authorize layout.
//! - Receiver SSA values survive holder traversal and all scratch work.
//! - Named loads omit only a receiver shape guard matching their live exact
//!   proof. Holder, prototype-validity and dictionary guards remain intact.
//! - Addresses carry exact symbolic relocations; hashes and slots retain width.
//! - Static programs prove eligible exact shapes or their admitted intrinsic
//!   dictionary contract. Shared probes use current ShapeState, including
//!   provisional feedback; direct stores reject prototype role before effects.
//!
//! # See also
//! - `otter_vm::JitCacheIrOp` owns the accepted property proof vocabulary.
//! - [`super::barriers`] owns committed value and child-shape edge barriers.
//! - [`crate::graph::metadata`] owns source-body and collector identities.

use dynasmrt::{DynamicLabel, DynasmApi, DynasmLabelApi, dynasm};
use otter_vm::{
    JitCacheIrOp as Op, ObjectProtocolValueOp, jit::JitIntrinsicPrototype, native_abi as abi,
    object::ShapeState,
};

use super::Codegen;
use crate::x86_64::values::emit_load_symbol_u64;
use crate::{
    Unsupported,
    artifact::relocation::{GuardedHeapComponent, RelocationTarget},
    graph::{
        call::CommittedArgument,
        ir::{DeoptReason, Kind, NamedStorePlan, NodeId},
    },
};

impl Codegen<'_> {
    /// `Rd(destination)` = the code unit of the string `Rq(receiver)` at the
    /// int32 index `Rd(index)`: a flat string (inline or sequential, Latin-1
    /// or UTF-16) or a slice of one, with the index inside it. Anything else
    /// jumps to `miss` before `Rd(destination)` changes. Clobbers `Rq(base)`,
    /// r10 and r11.
    fn emit_string_char_code_at(
        &mut self,
        [receiver, index, base]: [u8; 3],
        destination: u8,
        miss: DynamicLabel,
    ) {
        let p = self.view.string_layout;
        let map = p.map_atom_word as u32;
        let flat = self.ops.new_dynamic_label();
        let seq_latin1 = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        self.emit_cell_guard(receiver, miss);
        dynasm!(self.ops ; .arch x64
            ; cmp DWORD [Rq(receiver) + otter_vm::jit::JIT_MAP_WORD_BYTE as i32], map as i32 ; jne =>miss
            ; cmp Rd(index), [Rq(receiver) + p.string_len_byte as i32] ; jae =>miss
            ; mov Rq(base), Rq(receiver)
            ; mov r11d, Rd(index)
            ; movzx r10d, BYTE [Rq(receiver) + p.string_repr_byte as i32]
            ; cmp r10d, i32::from(p.sliced_tag) ; jne =>flat
            // A slice views its flat parent from a start offset.
            ; add r11d, [Rq(receiver) + p.sliced_start_byte as i32]
            ; mov r10d, [Rq(receiver) + p.sliced_parent_byte as i32]);
        self.load_immediate(base, 0xffff_ffff_0000_0000);
        dynasm!(self.ops ; .arch x64
            ; and Rq(base), Rq(receiver) ; add Rq(base), r10
            ; movzx r10d, BYTE [Rq(base) + p.string_repr_byte as i32]
            ; =>flat
            ; cmp r10d, i32::from(p.seq_latin1_tag) ; je =>seq_latin1
            ; cmp r10d, i32::from(p.seq_flat_tag) ; jne =>miss
            ; movzx Rd(destination), WORD [Rq(base) + r11 * 2 + p.string_body_size as i32]
            ; jmp =>done
            ; =>seq_latin1
            ; movzx Rd(destination), BYTE [Rq(base) + r11 + p.string_body_size as i32]
            ; =>done);
    }

    /// `Rq(destination)` = the own-property bag of the closure or native
    /// function `Rq(receiver)`, or the statics of the class constructor, whose
    /// named lookup starts there: the bag exists, a native has no
    /// `[[Prototype]]` override and the default realm is active. Anything else jumps to `miss` before `Rq(destination)`
    /// changes. Clobbers r10 and r11.
    fn emit_function_bag(&mut self, receiver: u8, destination: u8, miss: DynamicLabel) {
        let layout = self.view.closure_call_layout;
        let native = self.view.native_call_layout;
        // A closure's bag mirrors its [[Prototype]], override or not.
        let class = self.view.class_constructor_layout;
        let native_bag = self.ops.new_dynamic_label();
        let class_statics = self.ops.new_dynamic_label();
        let bag = self.ops.new_dynamic_label();
        self.emit_cell_guard(receiver, miss);
        dynasm!(self.ops ; .arch x64
            ; cmp BYTE [Rq(receiver)], otter_vm::native_function::NATIVE_FUNCTION_BODY_TYPE_TAG as i8
            ; je =>native_bag
            ; cmp BYTE [Rq(receiver)], class.type_tag as i8
            ; je =>class_statics
            ; cmp BYTE [Rq(receiver)], otter_vm::closure::JS_CLOSURE_BODY_TYPE_TAG as i8
            ; jne =>miss
            ; test BYTE [Rq(receiver) + otter_vm::closure::CLOSURE_NAMED_LOOKUP_BYTE as i32],
                otter_vm::closure::CLOSURE_LOOKUP_OWN_PROPS as i8
            ; jz =>miss
            ; mov r10d, [Rq(receiver) + layout.rare_byte as i32]
            ; test r10d, r10d ; jz =>miss);
        self.load_immediate(11, 0xffff_ffff_0000_0000);
        dynasm!(self.ops ; .arch x64
            ; and r11, Rq(receiver) ; add r10, r11
            ; mov r10d, [r10 + layout.own_props_byte as i32]
            ; jmp =>bag
            ; =>class_statics
            ; mov r10d, [Rq(receiver) + class.statics_byte as i32]
            ; jmp =>bag
            ; =>native_bag
            ; test BYTE [Rq(receiver) + native.flags_byte as i32], native.prototype_override_flag as i8
            ; jnz =>miss
            ; mov r10d, [Rq(receiver) + native.own_props_byte as i32]
            ; =>bag
            ; test r10d, r10d ; jz =>miss
            ; mov r11, [r15 + crate::entry::THREAD_OFFSET as i32]
            ; mov r11, [r11 + crate::entry::VM_THREAD_ACTIVE_REALM_CELL_OFFSET as i32]
            ; test r11, r11 ; jz =>miss
            ; cmp DWORD [r11], 0 ; jne =>miss);
        self.load_immediate(11, 0xffff_ffff_0000_0000);
        dynasm!(self.ops ; .arch x64
            ; and r11, Rq(receiver) ; add r11, r10 ; mov Rq(destination), r11);
    }

    pub(super) fn emit_properties(&mut self, node: NodeId) -> Result<bool, Unsupported> {
        let assigned = self.allocation.node(node).clone();
        let input = |i| Self::gp(assigned.inputs[i]);
        let result = || Self::gp(assigned.result.expect("property result"));
        match self.graph.node(node).kind.clone() {
            Kind::LoadArrayExpando(layout) => {
                let exit = self.eager_exit(node, DeoptReason::WrongShape);
                crate::x86_64::property_ic::emit_array_expando(
                    &mut self.ops,
                    layout,
                    input(0),
                    result(),
                    exit,
                );
            }
            Kind::LoadStringLength => {
                let exit = self.eager_exit(node, DeoptReason::WrongType);
                let p = self.view.string_layout;
                let (receiver, destination) = (input(0), result());
                self.emit_cell_guard(receiver, exit);
                dynasm!(self.ops ; .arch x64
                    ; cmp DWORD [Rq(receiver) + otter_vm::jit::JIT_MAP_WORD_BYTE as i32], p.map_atom_word as u32 as i32
                    ; jne =>exit
                    ; mov Rd(destination), [Rq(receiver) + p.string_len_byte as i32]);
            }
            Kind::StringCharCodeAt { code_point } => {
                let exit = self.eager_exit(node, DeoptReason::WrongType);
                self.emit_string_char_code_at(
                    [input(0), input(1), assigned.gp_temps[0]],
                    result(),
                    exit,
                );
                if code_point {
                    dynasm!(self.ops ; .arch x64
                        ; mov r10d, Rd(result())
                        ; and r10d, 0xfc00
                        ; cmp r10d, 0xd800
                        ; je =>exit);
                }
            }
            Kind::LoadFunctionBag => {
                let exit = self.eager_exit(node, DeoptReason::WrongShape);
                self.emit_function_bag(input(0), result(), exit);
            }
            Kind::LoadFunctionBagCell(cell) => {
                let exit = self.eager_exit(node, DeoptReason::WrongShape);
                dynasm!(self.ops ; .arch x64
                    ; mov r11, [r15 + crate::entry::THREAD_OFFSET as i32]
                    ; mov r11, [r11 + crate::entry::VM_THREAD_ACTIVE_REALM_CELL_OFFSET as i32]
                    ; test r11, r11 ; jz =>exit
                    ; cmp DWORD [r11], 0 ; jne =>exit);
                let target = RelocationTarget::CalleeIdentityCell {
                    function_id: self.view_of(node).code_block.id,
                    call_pc: self.graph.node(node).pc,
                };
                emit_load_symbol_u64(&mut self.ops, &mut self.relocations, 11, cell, target);
                dynasm!(self.ops ; .arch x64 ; mov Rq(result()), [r11]);
            }
            Kind::LoadShapedProperty { atom } => {
                let exit = self.eager_exit(node, DeoptReason::OwnDataMiss);
                let view = self.view_of(node);
                crate::x86_64::shape_lookup::emit_load(
                    &mut self.ops,
                    view,
                    atom,
                    input(0),
                    assigned.gp_temps[..4].try_into().unwrap(),
                    result(),
                    exit,
                );
            }
            Kind::LoadNamedProperty { byte_pc, .. } => self.emit_named_program(
                node,
                byte_pc,
                input(0),
                None,
                assigned.gp_temps[0],
                Some(result()),
            )?,
            Kind::StoreNamedProperty {
                byte_pc: pc, plan, ..
            } => self.emit_named_program(
                node,
                pc,
                input(0),
                Some(if plan.is_double() {
                    Self::fp(assigned.inputs[1])
                } else {
                    input(1)
                }),
                assigned.gp_temps[0],
                None,
            )?,
            Kind::StoreBodySlot(slot) => {
                if !slot.is_generated(true) {
                    return Err(Unsupported::OperandShape("x86 graph body slot store"));
                }
                let exit = self.eager_exit(node, DeoptReason::WrongShape);
                let (receiver, value) = (input(0), input(1));
                self.emit_body_slot_receiver(receiver, slot, exit);
                dynasm!(self.ops ; .arch x64 ; mov [Rq(receiver) + slot.byte as i32], Rq(value));
            }
            Kind::LoadPropertyCached { pc, length } => {
                let have_length = length.then(|| self.emit_exotic_length(node, input(0), result()));
                self.emit_property_probe_load(node, pc, input(0), result())?;
                if let Some(have_length) = have_length {
                    dynasm!(self.ops ; .arch x64 ; =>have_length);
                }
            }
            Kind::StorePropertyCached { pc } => {
                self.emit_property_routine_store(node, pc, [input(0), input(1)])?
            }
            Kind::LoadGuardedMethod {
                byte_pc,
                target,
                receiver_proved,
                ..
            } => {
                let guard = self
                    .view_of(node)
                    .method_targets
                    .get(&byte_pc)
                    .and_then(|methods| methods.get(usize::from(target)))
                    .ok_or(Unsupported::OperandShape("x86 graph guarded method"))?
                    .guard
                    .clone();
                // A proved receiver's cell read takes no input.
                let receiver = if assigned.inputs.is_empty() {
                    result()
                } else {
                    input(0)
                };
                let holder = assigned.gp_temps[0];
                if !receiver_proved {
                    let exit = self.eager_exit(node, DeoptReason::WrongShape);
                    self.emit_cell_guard(receiver, exit);
                    dynasm!(self.ops ; .arch x64 ; cmp DWORD [Rq(receiver) + otter_vm::jit::JIT_MAP_WORD_BYTE as i32], guard.recv_shape as i32 ; jne =>exit);
                }
                // The chain proof is a code dependency, tested before this
                // load: it fixes the holder slot, and the code-retained cell
                // holds the closure read there when the guard was baked.
                if guard.method_cell != 0 && guard.accessor_byte.is_none() {
                    emit_load_symbol_u64(
                        &mut self.ops,
                        &mut self.relocations,
                        result(),
                        guard.method_cell,
                        RelocationTarget::CalleeIdentityCell {
                            function_id: guard.method_fid,
                            call_pc: byte_pc,
                        },
                    );
                    dynasm!(self.ops ; .arch x64 ; mov Rq(result()), [Rq(result())]);
                    return Ok(true);
                }
                if guard.holder_root == 0 {
                    dynasm!(self.ops ; .arch x64 ; mov Rq(holder), Rq(receiver));
                } else {
                    let exit = self.eager_exit(node, DeoptReason::WrongShape);
                    self.emit_prototype_holder(receiver, holder, guard.holder_root, exit);
                }
                crate::x86_64::fields::emit_own_field(
                    &mut self.ops,
                    self.view.field_layout,
                    holder,
                    result(),
                    guard.method_field,
                    crate::field_access::FieldAccess::LoadTagged,
                );
                // An accessor's slot holds its cell; the method is the cell's
                // getter or setter. A cell value is its header's full address.
                if let Some(byte) = guard.accessor_byte {
                    dynasm!(self.ops ; .arch x64 ; mov Rq(result()), [Rq(result()) + byte as i32]);
                }
            }
            Kind::Instanceof => self.emit_instanceof(
                node,
                [input(0), input(1)],
                assigned.gp_temps[..5].try_into().unwrap(),
                result(),
            )?,
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Jump to `miss` unless `receiver` is a cell of the slot's type whose
    /// guard word holds.
    fn emit_body_slot_receiver(
        &mut self,
        receiver: u8,
        slot: otter_vm::jit::JitBodySlot,
        miss: DynamicLabel,
    ) {
        self.emit_cell_guard(receiver, miss);
        dynasm!(self.ops ; .arch x64 ; cmp BYTE [Rq(receiver)], slot.type_tag as i8 ; jne =>miss);
        if let Some(guard) = slot.guard {
            self.emit_body_guard(receiver, guard, miss);
        }
    }

    pub(super) fn emit_intrinsic_prototype(
        &mut self,
        target: JitIntrinsicPrototype,
        pc: u32,
        receiver: u8,
        holder: u8,
        miss: DynamicLabel,
        stub_id: u32,
    ) {
        self.emit_intrinsic_receiver(target, receiver, miss);
        emit_load_symbol_u64(
            &mut self.ops,
            &mut self.relocations,
            holder,
            u64::from(target.proto_offset),
            RelocationTarget::GuardedHeapReference {
                component: GuardedHeapComponent::Prototype,
                byte_pc: pc,
                runtime_stub_id: stub_id,
            },
        );
        self.load_immediate(10, 0xffff_ffff_0000_0000);
        dynasm!(self.ops ; .arch x64 ; and r10, Rq(receiver) ; add Rq(holder), r10);
    }

    /// Branch to `miss` unless `receiver` is a body of `target`'s family
    /// whose named lookup starts at its pinned prototype, in the realm the
    /// proof names. Clobbers `r10`.
    pub(super) fn emit_intrinsic_receiver(
        &mut self,
        target: JitIntrinsicPrototype,
        receiver: u8,
        miss: DynamicLabel,
    ) {
        if !target.is_generated_receiver() {
            dynasm!(self.ops ; .arch x64 ; jmp =>miss);
            return;
        }
        self.emit_cell_guard(receiver, miss);
        dynasm!(self.ops ; .arch x64 ; cmp BYTE [Rq(receiver)], target.type_tag as i8 ; jne =>miss);
        if let Some(guard) = target.guard {
            self.emit_body_guard(receiver, guard, miss);
        }
        if let Some(realm) = target.active_realm {
            dynasm!(self.ops ; .arch x64
                ; mov r10, [r15 + crate::entry::THREAD_OFFSET as i32]
                ; mov r10, [r10 + crate::entry::VM_THREAD_ACTIVE_REALM_CELL_OFFSET as i32]
                ; test r10, r10 ; jz =>miss ; cmp DWORD [r10], realm as i32 ; jne =>miss);
        }
    }

    fn emit_validity_guard(
        &mut self,
        validity: otter_vm::jit::JitPrototypeValidity,
        miss: DynamicLabel,
    ) {
        emit_load_symbol_u64(
            &mut self.ops,
            &mut self.relocations,
            10,
            validity.address as u64,
            RelocationTarget::PrototypeValidityCell {
                identity: validity.identity,
            },
        );
        dynasm!(self.ops ; .arch x64 ; cmp DWORD [r10], 0 ; je =>miss);
    }

    fn emit_prototype_holder(&mut self, receiver: u8, holder: u8, root: u32, miss: DynamicLabel) {
        self.load_immediate(10, 0xffff_ffff_0000_0000);
        self.load_immediate(11, u64::from(root));
        dynasm!(self.ops ; .arch x64 ; and r10, Rq(receiver) ; add r11, r10
            ; mov Rd(holder), [r11 + self.view.shape_prototype_byte as i32]
            ; test Rd(holder), Rd(holder) ; jz =>miss ; add Rq(holder), r10);
    }

    fn emit_named_program(
        &mut self,
        node: NodeId,
        pc: u32,
        receiver: u8,
        value: Option<u8>,
        holder: u8,
        destination: Option<u8>,
    ) -> Result<(), Unsupported> {
        let (receiver_shape, proved) = match self.graph.node(node).kind {
            Kind::StoreNamedProperty {
                receiver_shape,
                proved,
                ..
            } => (receiver_shape, proved),
            _ => (None, false),
        };
        let programs = crate::graph::feedback::store_node_programs(
            self.view_of(node),
            pc,
            receiver_shape,
            proved,
        )
        .ok_or(Unsupported::OperandShape("x86 graph named programs"))?
        .to_vec();
        let (selected, receiver_shape, owners, fresh, plan) =
            match self.graph.node(node).kind.clone() {
                Kind::LoadNamedProperty {
                    programs,
                    receiver_shape,
                    ..
                } => (
                    programs,
                    receiver_shape,
                    smallvec::SmallVec::new(),
                    false,
                    NamedStorePlan::Programs,
                ),
                Kind::StoreNamedProperty {
                    owners,
                    receiver_shape,
                    fresh,
                    plan,
                    ..
                } if owners.len() == programs.len() => (
                    // A proved receiver matches only the programs that guard its
                    // shape.
                    (0..programs.len())
                        .filter(|&index| {
                            plan.program()
                                .is_none_or(|selected| usize::from(selected) == index)
                                && (receiver_shape.is_none()
                                    || programs[index].ops.iter().find_map(|op| match *op {
                                        Op::GuardShape { object: 0, shape } => Some(shape),
                                        _ => None,
                                    }) == receiver_shape)
                        })
                        .map(|index| index as u32)
                        .collect(),
                    receiver_shape,
                    owners,
                    fresh,
                    plan,
                ),
                _ => return Err(Unsupported::OperandShape("x86 graph named program plan")),
            };
        if plan.program().is_some_and(|index| {
            programs
                .get(usize::from(index))
                .is_none_or(|program| !crate::graph::feedback::inline_store_transition(program))
        }) {
            return Err(Unsupported::OperandShape(
                "x86 graph selected store program",
            ));
        }
        let exit = self.eager_exit(node, DeoptReason::WrongShape);
        let done = self.ops.new_dynamic_label();
        // A proved receiver that lacks the suffix storage an append needs.
        let grow = (value.is_some() && plan.grows()).then(|| self.ops.new_dynamic_label());
        let view = self.view;
        let stored_value = value.map(|value| {
            if plan.is_double() {
                self.allocation.node(node).gp_temps[2]
            } else {
                value
            }
        });
        // The validity cell a fresh receiver's allocation checked still holds:
        // no code ran since.
        let allocation = self.graph.node(node).inputs[0];
        let allocation_validity = match &self.graph.node(allocation).kind {
            Kind::NewReceiver(plan) if fresh => plan.prototype_validity.map(|cell| cell.address),
            _ => None,
        };
        // The baker exports no store program for a prototype-role shape, nor
        // an append for one that cannot grow: when every emitted program
        // guards the receiver's exact shape, that guard alone proves both,
        // and the map word it compares rules out every other cell kind.
        let shape_guarded = value.is_some()
            && crate::graph::feedback::store_programs_guard_receiver(
                selected.iter().map(|&index| &programs[index as usize]),
            );
        if value.is_some() {
            // Every successful static path proves an eligible receiver shape
            // before its field/effect. Prototype writes additionally need the
            // canonical invalidation owner and cannot commit this direct hit.
            if receiver_shape.is_none() {
                if shape_guarded {
                    self.emit_cell_guard(receiver, exit);
                } else {
                    self.emit_object_receiver(receiver, exit);
                }
            }
            // A fresh receiver ran no code since its allocation, so it has no
            // prototype role.
            if !fresh && !shape_guarded {
                self.emit_shape_state(receiver);
                dynasm!(self.ops ; .arch x64 ; test r10b, ShapeState::PROTOTYPE_MASK as i8 ; jnz =>exit);
            }
        }
        let last = selected.last().copied();
        for index in selected {
            let program = programs
                .get(index as usize)
                .ok_or(Unsupported::OperandShape("x86 graph named program index"))?;
            // The last program's misses leave; it falls through to `done`.
            let final_program = Some(index) == last;
            let expando = matches!(
                program.ops.first(),
                Some(Op::LoadArrayExpando {
                    object: 0,
                    result: 1,
                    ..
                })
            );
            let known = owners.get(index as usize);
            let mut owner = known.map_or(0, |record| record.owner_shape);
            let next = if final_program {
                exit
            } else {
                self.ops.new_dynamic_label()
            };
            let object = |operand| {
                if operand == 0 || (expando && value.is_some()) {
                    receiver
                } else {
                    holder
                }
            };
            let mut committed = false;
            for op in program.ops.iter() {
                match *op {
                    Op::LoadArrayExpando { layout, .. } if value.is_none() => {
                        crate::x86_64::property_ic::emit_array_expando(
                            &mut self.ops,
                            layout,
                            receiver,
                            holder,
                            next,
                        );
                    }
                    Op::LoadArrayExpando { .. } if expando && value.is_some() => {}
                    Op::LoadClosurePrototype { object: 0 } if value.is_none() => {
                        crate::template::x86_64::emit_closure_prototype_load(
                            &mut self.ops,
                            view,
                            receiver,
                            destination.expect("named load result"),
                            next,
                        );
                        dynasm!(self.ops ; .arch x64 ; jmp =>done);
                        committed = true;
                    }
                    Op::LoadBodySlot { object: 0, slot }
                        if value.is_none() && slot.is_generated(false) =>
                    {
                        let destination = destination.expect("named load result");
                        self.emit_body_slot_receiver(receiver, slot, next);
                        dynasm!(self.ops ; .arch x64
                            ; mov Rq(destination), [Rq(receiver) + slot.byte as i32]
                        );

                        dynasm!(self.ops ; .arch x64 ; jmp =>done);
                        committed = true;
                    }
                    Op::LoadIntrinsicPrototype { target, .. } if value.is_none() => self
                        .emit_intrinsic_prototype(
                            target,
                            pc,
                            receiver,
                            holder,
                            next,
                            abi::STUB_JIT_LOAD_PROPERTY.id,
                        ),
                    Op::LoadTypedArrayStart {
                        object: 0,
                        result: 1,
                    } if value.is_none() => {
                        // A typed view whose body records its lookup start.
                        let typed = view.typed_array_length;
                        if typed.lookup_start_byte == 0 {
                            return Err(Unsupported::OperandShape(
                                "x86 graph typed array start layout",
                            ));
                        }
                        self.emit_cell_guard(receiver, next);
                        dynasm!(self.ops ; .arch x64
                            ; cmp BYTE [Rq(receiver)], typed.type_tag as i8 ; jne =>next
                            ; mov Rd(holder), [Rq(receiver) + typed.lookup_start_byte as i32]
                            ; test Rd(holder), Rd(holder) ; jz =>next
                        );
                        self.load_immediate(11, 0xffff_ffff_0000_0000);
                        dynasm!(self.ops ; .arch x64 ; and r11, Rq(receiver) ; add Rq(holder), r11);
                    }
                    Op::LoadPrototypeHolder { root, .. } => {
                        self.emit_prototype_holder(receiver, holder, root, next)
                    }
                    Op::GuardPrototypeValidity { validity }
                        if allocation_validity != Some(validity.address) =>
                    {
                        self.emit_validity_guard(validity, next)
                    }
                    Op::GuardPrototypeValidity { .. } => {}
                    Op::GuardShape {
                        object: operand,
                        shape,
                    } => {
                        if operand == 0 && receiver_shape == Some(shape) {
                            continue;
                        }
                        let header = object(operand);
                        if operand == 0 {
                            self.emit_cell_guard(header, next);
                            dynasm!(self.ops ; .arch x64 ; cmp BYTE [Rq(header)], crate::entry::OBJECT_BODY_TYPE_TAG as i8 ; jne =>next);
                        }
                        // Finalized shape identity fixes ordinary lookup state
                        // and immutable descriptor/atom/slot authority.
                        dynasm!(self.ops ; .arch x64
                            ; cmp DWORD [Rq(header) + view.object_shape_byte as i32], shape as i32 ; jne =>next);
                    }
                    Op::GuardDictionaryLayout { layout, .. } if value.is_none() => {
                        self.load_immediate(11, 0xffff_ffff_0000_0000);
                        dynasm!(self.ops ; .arch x64 ; and r11, Rq(holder)
                            ; mov r10d, [Rq(holder) + view.object_shape_byte as i32] ; add r10, r11
                            ; test BYTE [r10 + view.shape_state_byte as i32], ShapeState::DICTIONARY_MASK as i8 ; jz =>next
                            ; mov r10d, [Rq(holder) + view.object_exotic_handle_byte as i32]
                            ; test r10d, r10d ; jz =>next ; add r10, r11
                            ; cmp DWORD [r10 + view.exotic_dictionary_layout_byte as i32], layout as u32 as i32 ; jne =>next);
                    }
                    Op::GuardPrototypeNull { object: operand } if value.is_some() => {
                        let header = object(operand);
                        self.load_immediate(11, 0xffff_ffff_0000_0000);
                        dynasm!(self.ops ; .arch x64 ; and r11, Rq(header));
                        crate::x86_64::fields::emit_load_prototype(
                            &mut self.ops,
                            view,
                            10,
                            header,
                            11,
                        );
                        dynasm!(self.ops ; .arch x64 ; test r10d, r10d ; jnz =>next);
                    }
                    Op::GuardAtomSlot { .. } => {}
                    Op::GuardExtensible { field, .. } if value.is_some() => {
                        // Storage the receiver lacks the committed store
                        // allocates.
                        if !field.is_inline() {
                            let short = grow.unwrap_or(next);
                            self.load_immediate(11, 0xffff_ffff_0000_0000);
                            dynasm!(self.ops ; .arch x64 ; and r11, Rq(receiver)
                                ; mov r10d, [Rq(receiver) + view.field_layout.slab_handle_byte as i32]
                                ; test r10d, r10d ; jz =>short ; add r10, r11
                                ; cmp DWORD [r10 + view.field_layout.slab_capacity_byte as i32], field.index() as i32 ; jbe =>short);
                        }
                        // A fresh receiver ran no code since its allocation,
                        // so it is still extensible; an exact guarded shape
                        // was exported extensible.
                        if !fresh && !shape_guarded {
                            self.emit_shape_state(receiver);
                            dynasm!(self.ops ; .arch x64 ; test r10b, ShapeState::EXTENSIBLE_MASK as i8 ; jz =>next);
                        }
                    }
                    Op::LoadField {
                        object: operand,
                        field,
                    } if value.is_none() => {
                        crate::x86_64::fields::emit_own_field(
                            &mut self.ops,
                            view.field_layout,
                            object(operand),
                            destination.expect("named load result"),
                            field,
                            crate::field_access::FieldAccess::LoadTagged,
                        );

                        committed = true;
                        dynasm!(self.ops ; .arch x64 ; jmp =>done);
                    }
                    Op::StoreField { field, .. } if value.is_some() => {
                        let physical = stored_value.expect("named store value");
                        if plan.is_double() {
                            crate::x86_64::field_word::emit_encode_double(
                                &mut self.ops,
                                value.unwrap(),
                                physical,
                                10,
                            );
                        }
                        crate::x86_64::fields::emit_own_field(
                            &mut self.ops,
                            view.field_layout,
                            receiver,
                            physical,
                            field,
                            crate::field_access::FieldAccess::Store,
                        );
                        committed = true;
                    }
                    Op::PublishShape { shape, .. } if value.is_some() => {
                        // The object leaves its shape. One some object had
                        // already left when the generation compiled stays
                        // unstable; any other is retired here.
                        let parent = program.ops.iter().find_map(|op| match *op {
                            Op::GuardShape { object: 0, shape } => Some(shape),
                            _ => None,
                        });
                        if !parent.is_some_and(|parent| view.left_shapes.contains(&parent)) {
                            self.emit_leave_receiver_shape(
                                node,
                                receiver,
                                stored_value.expect("named store value"),
                            );
                        }
                        // Source-program baking retains this pinned target
                        // until its native generation physically retires.
                        dynasm!(self.ops ; .arch x64
                            ; mov DWORD [Rq(receiver) + view.object_shape_byte as i32], shape as i32);
                        if let Some(record) = known.filter(|_| owner != 0) {
                            self.load_immediate(holder, u64::from(owner));
                            crate::x86_64::field_type::emit_observe_known(
                                &mut self.ops,
                                &mut self.relocations,
                                self.routines,
                                view,
                                holder,
                                stored_value.expect("named store value"),
                                record,
                            );
                            owner = 0;
                        }
                        // The published shape is an old, nonmoving cell the
                        // installed code retains as a strong root, which every
                        // marking cycle shades at its start and finish: the
                        // edge owes neither barrier half.
                    }
                    _ => {
                        return Err(Unsupported::OperandShape(
                            "x86 graph named program operation",
                        ));
                    }
                }
            }
            if !committed {
                return Err(Unsupported::OperandShape(
                    "x86 graph named program without access",
                ));
            }
            if let Some(record) = known.filter(|_| owner != 0) {
                self.load_immediate(holder, u64::from(owner));
                crate::x86_64::field_type::emit_observe_known(
                    &mut self.ops,
                    &mut self.relocations,
                    self.routines,
                    view,
                    holder,
                    stored_value.expect("named store value"),
                    record,
                );
            }
            if !final_program {
                dynasm!(self.ops ; .arch x64 ; jmp =>done ; =>next);
            }
        }
        if last.is_none() {
            dynasm!(self.ops ; .arch x64 ; jmp =>exit);
        }
        if let (Some(grow), Some(value)) = (grow, value) {
            // Every guard before the append held, so the committed store
            // makes the same append, allocating the storage, and runs no
            // JavaScript.
            let (slot, target) = self.property_ic_slot(node, pc)?;
            self.defer(move |codegen| {
                dynasm!(codegen.ops ; .arch x64 ; =>grow);
                codegen.emit_committed_call(
                    node,
                    abi::STUB_JIT_STORE_PROPERTY,
                    &[
                        CommittedArgument::Value(receiver),
                        CommittedArgument::Value(value),
                        CommittedArgument::Address(slot, target),
                    ],
                    None,
                )?;
                dynasm!(codegen.ops ; .arch x64 ; jmp =>done);
                Ok(())
            });
        }
        dynasm!(self.ops ; .arch x64 ; =>done);
        Ok(())
    }

    /// An ordinary array's, a string's or a plain typed view's `length` as a
    /// tagged int32, then a jump to the returned label, bound after the node;
    /// any other receiver, or a length past int32, falls through to the
    /// property load emitted after it.
    fn emit_exotic_length(&mut self, node: NodeId, receiver: u8, destination: u8) -> DynamicLabel {
        let view = self.view_of(node);
        let array_tag = view.array_layout.type_tag as i8;
        let array_length_byte = view.array_layout.length_byte as i32;
        let string_tag = view.string_layout.string_type_tag as i8;
        let string_length_byte = view.string_layout.string_len_byte as i32;
        let typed = view.typed_array_length;
        let (string, typed_view, boxed, other, done) = (
            self.ops.new_dynamic_label(),
            self.ops.new_dynamic_label(),
            self.ops.new_dynamic_label(),
            self.ops.new_dynamic_label(),
            self.ops.new_dynamic_label(),
        );
        self.emit_cell_guard(receiver, other);
        dynasm!(self.ops
            ; .arch x64
            ; cmp BYTE [Rq(receiver)], array_tag
            ; jne =>string
            ; mov r10d, DWORD [Rq(receiver) + array_length_byte]
            ; jmp =>boxed
            ; =>string
            ; cmp BYTE [Rq(receiver)], string_tag
            ; jne =>typed_view
            ; mov r10d, DWORD [Rq(receiver) + string_length_byte]
            ; jmp =>boxed
            ; =>typed_view
        );
        if typed.type_tag == 0 {
            dynasm!(self.ops ; .arch x64 ; jmp =>other);
        } else {
            // A typed view's count is its `length` while it owns no named
            // property, its storage is a fixed-length local buffer and no
            // buffer was ever detached.
            dynasm!(self.ops
                ; .arch x64
                ; cmp BYTE [Rq(receiver)], typed.type_tag as i8
                ; jne =>other
                ; cmp DWORD [Rq(receiver) + typed.expando_byte as i32], 0
                ; jne =>other
                ; cmp QWORD [Rq(receiver) + typed.data_byte as i32], 0
                ; je =>other
                ; mov r10, [r15 + crate::entry::THREAD_OFFSET as i32]
                ; mov r10, [r10 + crate::entry::VM_THREAD_ARRAY_BUFFER_DETACH_PROTECTOR_CELL_OFFSET as i32]
                ; test r10, r10 ; jz =>other
                ; cmp BYTE [r10], 0 ; jne =>other
                ; mov r10, QWORD [Rq(receiver) + typed.length_byte as i32]
            );
        }
        dynasm!(self.ops
            ; .arch x64
            ; =>boxed
            // A length past int32 is a Number the property load boxes.
            ; cmp r10, i32::MAX
            ; ja =>other
        );
        crate::x86_64::values::emit_box_int32(&mut self.ops, 10, destination, 11);
        dynasm!(self.ops ; .arch x64 ; jmp =>done ; =>other);
        done
    }

    /// The IC slot of the named-property site at `pc` of the body `node`
    /// belongs to, with its relocation.
    fn property_site_slot(
        &self,
        node: NodeId,
        pc: u32,
    ) -> Result<(u64, RelocationTarget), Unsupported> {
        let byte_pc = self
            .view_of(node)
            .instructions
            .get(pc as usize)
            .map(|instruction| instruction.byte_pc)
            .ok_or(Unsupported::OperandShape("x86 graph property site pc"))?;
        self.property_ic_slot(node, byte_pc)
    }

    /// The IC slot of the property site at `byte_pc` in `node`'s function.
    fn property_ic_slot(
        &self,
        node: NodeId,
        byte_pc: u32,
    ) -> Result<(u64, RelocationTarget), Unsupported> {
        let view = self.view_of(node);
        let slot = view
            .property_accesses
            .get(&byte_pc)
            .map(|access| access.ic_slot)
            .filter(|&slot| slot != 0)
            .ok_or(Unsupported::OperandShape(
                "x86 graph property site without an IC slot",
            ))?;
        Ok((
            slot,
            RelocationTarget::PropertyIcSlot {
                function_id: view.code_block.id,
                byte_pc,
            },
        ))
    }

    /// `Rq(destination)` = the named property at `pc` of `Rq(receiver)`,
    /// both in the probe routine's fixed words: the routine's selection,
    /// which preserves every other register, and on its miss the committed
    /// runtime operation with every live register kept in its canonical home.
    fn emit_property_probe_load(
        &mut self,
        node: NodeId,
        pc: u32,
        receiver: u8,
        destination: u8,
    ) -> Result<(), Unsupported> {
        use crate::template::x86_64::shared_property::{PROBE_RECEIVER, PROBE_SLOT, PROBE_STATUS};
        debug_assert_eq!(receiver, PROBE_RECEIVER);
        debug_assert_eq!(
            destination,
            super::super::registers::X86_64.property_probe.result
        );
        let (slot, target) = self.property_site_slot(node, pc)?;
        let done = self.ops.new_dynamic_label();
        emit_load_symbol_u64(
            &mut self.ops,
            &mut self.relocations,
            PROBE_SLOT,
            slot,
            target.clone(),
        );
        crate::template::x86_64::shared_property::emit_call(
            &mut self.ops,
            &mut self.relocations,
            self.routines,
            crate::routines::Routine::ProbeLoad,
        );
        dynasm!(self.ops ; .arch x64 ; test Rq(PROBE_STATUS), Rq(PROBE_STATUS) ; jz =>done);
        self.emit_committed_call(
            node,
            abi::STUB_JIT_LOAD_PROPERTY,
            &[
                CommittedArgument::Value(receiver),
                CommittedArgument::Address(slot, target),
            ],
            Some(destination),
        )?;
        dynasm!(self.ops ; .arch x64 ; =>done);
        Ok(())
    }

    /// The named store at `pc` of `Rq(value)` into `Rq(receiver)`, both in
    /// the store probe's fixed words: the probe's selection, which preserves
    /// every allocatable register, and on its miss the committed runtime
    /// operation with every live register kept in its canonical home.
    fn emit_property_routine_store(
        &mut self,
        node: NodeId,
        pc: u32,
        [receiver, value]: [u8; 2],
    ) -> Result<(), Unsupported> {
        use crate::template::x86_64::shared_property::{
            PROBE_SLOT, PROBE_STATUS, PROBE_STORE_RECEIVER, PROBE_STORE_VALUE,
        };
        debug_assert_eq!((receiver, value), (PROBE_STORE_RECEIVER, PROBE_STORE_VALUE));
        let (slot, target) = self.property_site_slot(node, pc)?;
        let done = self.ops.new_dynamic_label();
        emit_load_symbol_u64(
            &mut self.ops,
            &mut self.relocations,
            PROBE_SLOT,
            slot,
            target.clone(),
        );
        crate::template::x86_64::shared_property::emit_call(
            &mut self.ops,
            &mut self.relocations,
            self.routines,
            crate::routines::Routine::ProbeStore,
        );
        dynasm!(self.ops ; .arch x64 ; test Rq(PROBE_STATUS), Rq(PROBE_STATUS) ; jz =>done);
        self.emit_committed_call(
            node,
            abi::STUB_JIT_STORE_PROPERTY,
            &[
                CommittedArgument::Value(receiver),
                CommittedArgument::Value(value),
                CommittedArgument::Address(slot, target),
            ],
            None,
        )?;
        dynasm!(self.ops ; .arch x64 ; =>done);
        Ok(())
    }

    fn emit_instanceof(
        &mut self,
        node: NodeId,
        [value, target]: [u8; 2],
        [cage, rare, prototype, cursor, budget]: [u8; 5],
        destination: u8,
    ) -> Result<(), Unsupported> {
        let view = self.view_of(node);
        let layout = view.closure_call_layout;
        let miss = self.ops.new_dynamic_label();
        let yes = self.ops.new_dynamic_label();
        let no = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        let symbols_absent = self.ops.new_dynamic_label();
        let proved = self.ops.new_dynamic_label();
        let validate = self.ops.new_dynamic_label();
        let walk = self.ops.new_dynamic_label();
        let step = self.ops.new_dynamic_label();
        let pc = self.graph.node(node).pc;
        let cell = view.instructions.get(pc as usize).and_then(|instruction| {
            let byte_pc = instruction.byte_pc;
            view.instanceof_cells.get(&byte_pc).map(|&address| {
                (
                    address,
                    RelocationTarget::InstanceofCell {
                        function_id: view.code_block.id,
                        byte_pc,
                    },
                )
            })
        });
        let target_byte = otter_vm::jit::JIT_INSTANCEOF_CELL_TARGET_OFFSET as i32;
        let prototype_byte = otter_vm::jit::JIT_INSTANCEOF_CELL_PROTOTYPE_OFFSET as i32;
        if view.cage_base == 0 || layout.prototype_byte == 0 {
            dynasm!(self.ops ; .arch x64 ; jmp =>miss);
        } else {
            emit_load_symbol_u64(
                &mut self.ops,
                &mut self.relocations,
                cage,
                view.cage_base as u64,
                RelocationTarget::GcCageBase,
            );
            if let Some((address, relocation)) = cell.clone() {
                // The target this site last proved, and its prototype.
                emit_load_symbol_u64(
                    &mut self.ops,
                    &mut self.relocations,
                    10,
                    address,
                    relocation,
                );
                dynasm!(self.ops ; .arch x64
                    ; cmp Rq(target), [r10 + target_byte] ; jne =>validate
                    ; mov Rq(prototype), [r10 + prototype_byte] ; jmp =>proved
                    ; =>validate);
            }
            self.emit_cell_guard(target, miss);
            dynasm!(self.ops ; .arch x64
                ; mov Rd(rare), Rd(target) ; add Rq(rare), Rq(cage)
                ; cmp BYTE [Rq(rare)], otter_vm::closure::JS_CLOSURE_BODY_TYPE_TAG as i8 ; jne =>miss
                ; movzx r10d, BYTE [Rq(rare) + otter_vm::closure::CLOSURE_NAMED_LOOKUP_BYTE as i32]
                ; and r10d, !u32::from(otter_vm::closure::CLOSURE_LOOKUP_OWN_PROPS) as i32
                ; cmp r10d, otter_vm::closure::CLOSURE_LOOKUP_ORDINARY as i32 ; jne =>miss
                ; mov Rd(rare), [Rq(rare) + layout.rare_byte as i32] ; test Rd(rare), Rd(rare) ; jz =>miss
                ; add Rq(rare), Rq(cage)
                // Any `prototype` but an ordinary object completes in the runtime.
                ; cmp BYTE [Rq(rare) + layout.prototype_ordinary_byte as i32], 0 ; je =>miss
                ; mov Rq(prototype), [Rq(rare) + layout.prototype_byte as i32]
                ; mov r10d, [Rq(rare) + layout.own_props_byte as i32] ; test r10d, r10d ; jz =>symbols_absent
                ; add r10, Rq(cage)
                // A symbol key in the bag's hidden class or side table may
                // be `@@hasInstance`.
                ; mov r11d, [r10 + view.object_shape_byte as i32] ; add r11, Rq(cage)
                ; cmp BYTE [r11 + otter_vm::object::SHAPE_SYMBOL_KEYS_BYTE as i32], 0 ; jne =>miss
                ; mov r10d, [r10 + view.object_exotic_handle_byte as i32]
                ; test r10d, r10d ; jz =>proved ; add r10, Rq(cage)
                ; cmp DWORD [r10 + otter_vm::object::EXOTIC_SLOTS_SYMBOL_PROPS_BYTE as i32], 0 ; jne =>miss
                ; jmp =>proved
                ; =>symbols_absent);
            if let Some((address, relocation)) = cell {
                // A closure without own properties is cached; the flag asks
                // the VM to empty the cell before the closure changes.
                emit_load_symbol_u64(
                    &mut self.ops,
                    &mut self.relocations,
                    10,
                    address,
                    relocation,
                );
                dynasm!(self.ops ; .arch x64
                    ; mov [r10 + target_byte], Rq(target)
                    ; mov [r10 + prototype_byte], Rq(prototype)
                    ; mov BYTE [Rq(rare) + layout.instanceof_cached_byte as i32], 1);
            }
            dynasm!(self.ops ; .arch x64 ; =>proved);
            self.emit_cell_guard(value, no);
            dynasm!(self.ops ; .arch x64 ; mov Rd(cursor), Rd(value) ; add Rq(cursor), Rq(cage)
                ; movzx r10d, BYTE [Rq(cursor)] ; cmp r10d, crate::entry::OBJECT_BODY_TYPE_TAG as i32 ; je =>walk);
            for primitive in view.primitive_cell_type_tags {
                dynasm!(self.ops ; .arch x64 ; cmp r10d, primitive as i32 ; je =>no);
            }
            dynasm!(self.ops ; .arch x64 ; jmp =>miss
                ; =>walk ; mov Rd(budget), crate::graph::INSTANCEOF_CHAIN_BOUND as i32
                ; =>step);
            self.emit_shape_state(cursor);
            dynasm!(self.ops ; .arch x64 ; test r10b, ShapeState::OPAQUE_LOOKUP_MASK as i8 ; jnz =>miss);
            crate::x86_64::fields::emit_load_prototype(&mut self.ops, view, 10, cursor, cage);
            dynasm!(self.ops ; .arch x64 ; test r10d, r10d ; jz =>no
                ; lea Rq(cursor), [Rq(cage) + r10] ; cmp Rq(cursor), Rq(prototype) ; je =>yes
                ; cmp BYTE [Rq(cursor)], crate::entry::OBJECT_BODY_TYPE_TAG as i8 ; jne =>miss
                ; dec Rd(budget) ; jnz =>step ; jmp =>miss);
        }
        self.emit_boolean_outcomes([yes, no], destination, done);
        dynasm!(self.ops ; .arch x64 ; =>miss);
        // A native target (a builtin constructor), or a closure whose
        // `@@hasInstance` chain is the default (a re-parented constructor),
        // answers through the leaf probe without running JavaScript; any
        // other case completes in the runtime. The probe keeps every live value and both operands.
        let mut saved = self.loc(node).live_registers.clone();
        for register in [value, target] {
            if !saved
                .iter()
                .any(|(location, _)| *location == super::Location::Gp(register))
            {
                saved.push((super::Location::Gp(register), super::Repr::Tagged));
            }
        }
        let runtime = self.ops.new_dynamic_label();
        let bytes = self.emit_save_registers(&saved);
        dynasm!(self.ops ; .arch x64
            ; mov r10, Rq(value) ; mov r11, Rq(target)
            ; mov rsi, r10 ; mov rdx, r11
            ; mov rdi, [r15 + crate::entry::THREAD_OFFSET as i32]
            ; mov rdi, [rdi + crate::entry::VM_THREAD_GC_HEAP_OFFSET as i32]
        );
        self.emit_scalar_vm_leaf(
            abi::STUB_INSTANCEOF_LEAF,
            otter_vm::runtime_stubs::INSTANCEOF_LEAF.entry_addr() as u64,
        );
        dynasm!(self.ops ; .arch x64 ; mov r10, rax ; mov r11, rdx);
        self.emit_restore_registers(&saved, bytes);
        dynasm!(self.ops ; .arch x64
            ; test r11, r11 ; jnz =>runtime
            ; mov Rq(destination), r10 ; jmp =>done
            ; =>runtime
        );
        self.emit_committed_call(
            node,
            abi::STUB_JIT_OBJECT_PROTOCOL_VALUE,
            &[
                CommittedArgument::Value(value),
                CommittedArgument::Value(target),
                CommittedArgument::Scalar(ObjectProtocolValueOp::Instanceof.encode_abi()),
            ],
            Some(destination),
        )?;
        dynasm!(self.ops ; .arch x64 ; =>done);
        self.emit_sink_register(destination);
        Ok(())
    }
}
