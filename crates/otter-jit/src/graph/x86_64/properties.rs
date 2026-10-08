//! x86-64 Graph named properties, shared caches and ordinary prototype walks.
//!
//! # Contents
//! - Current CacheIR load/store programs and guarded method holders.
//! - One authoritative property-action table with live logical-slot banks.
//! - Source-owned committed property calls and default instanceof walks.
//!
//! # Invariants
//! - Every store guard completes before the first slot or shape write.
//! - Cold operations run once with canonical roots, never by replaying a hit.
//! - Mutable fields are read live; shape and chain identities authorize layout.
//! - Receiver SSA values survive holder traversal and all scratch work.
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
    JitCacheIrOp as Op, jit::JitIntrinsicPrototype, native_abi as abi, object::ShapeState,
    value::tag,
};

use super::Codegen;
use crate::x86_64::{property_actions::emit_action_probe, values::emit_load_symbol_u64};
use crate::{
    Unsupported,
    artifact::relocation::{GuardedHeapComponent, PropertySourceAccess, RelocationTarget},
    graph::{
        call::CommittedArgument,
        ir::{DeoptReason, Kind, NodeId},
    },
};

impl Codegen<'_> {
    pub(super) fn emit_properties(&mut self, node: NodeId) -> Result<bool, Unsupported> {
        let assigned = self.allocation.node(node).clone();
        let input = |i| Self::gp(assigned.inputs[i]);
        let result = || Self::gp(assigned.result.expect("property result"));
        match self.graph.node(node).kind.clone() {
            Kind::LoadNamedProperty(pc) => self.emit_named_program(
                node,
                pc,
                input(0),
                None,
                [assigned.gp_temps[0], assigned.gp_temps[1]],
                Some(result()),
            )?,
            Kind::StoreNamedProperty(pc) => self.emit_named_program(
                node,
                pc,
                input(0),
                Some(input(1)),
                [assigned.gp_temps[0], assigned.gp_temps[1]],
                None,
            )?,
            Kind::LoadPropertyCached { pc, atom, length } => {
                let have_length = length.then(|| self.emit_exotic_length(node, input(0), result()));
                self.emit_load_property_cached(
                    node,
                    pc,
                    atom,
                    input(0),
                    assigned.gp_temps[..4].try_into().unwrap(),
                    result(),
                )?;
                if let Some(have_length) = have_length {
                    dynasm!(self.ops ; .arch x64 ; =>have_length);
                }
            }
            Kind::StorePropertyCached { pc, atom } => self.emit_store_property_cached(
                node,
                pc,
                atom,
                [input(0), input(1)],
                assigned.gp_temps[..4].try_into().unwrap(),
            )?,
            Kind::LoadGuardedMethod {
                byte_pc,
                target,
                receiver_proved,
            } => {
                let guard = self
                    .view_of(node)
                    .direct_methods
                    .get(&byte_pc)
                    .and_then(|methods| methods.get(usize::from(target)))
                    .ok_or(Unsupported::OperandShape("x86 graph guarded method"))?
                    .guard
                    .clone();
                let receiver = input(0);
                let holder = assigned.gp_temps[0];
                let exit = self.eager_exit(node, DeoptReason::WrongShape);
                if !receiver_proved {
                    self.emit_object_receiver(receiver, exit);
                    dynasm!(self.ops ; .arch x64 ; cmp DWORD [Rq(receiver) + self.view.object_shape_byte as i32], guard.recv_shape as i32 ; jne =>exit);
                }
                if let Some(validity) = guard.prototype_validity {
                    self.emit_validity_guard(validity, exit);
                }
                if guard.holder_root == 0 {
                    dynasm!(self.ops ; .arch x64 ; mov Rq(holder), Rq(receiver));
                } else {
                    self.emit_prototype_holder(receiver, holder, guard.holder_root, exit);
                }
                crate::x86_64::fields::emit_own_field(
                    &mut self.ops,
                    self.view.field_layout,
                    holder,
                    result(),
                    guard.method_field,
                    false,
                );
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

    pub(super) fn emit_intrinsic_prototype(
        &mut self,
        target: JitIntrinsicPrototype,
        pc: u32,
        receiver: u8,
        holder: u8,
        miss: DynamicLabel,
        stub_id: u32,
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
        [holder, scratch]: [u8; 2],
        destination: Option<u8>,
    ) -> Result<(), Unsupported> {
        let programs = self
            .view_of(node)
            .property_programs
            .get(&pc)
            .ok_or(Unsupported::OperandShape("x86 graph named programs"))?
            .clone();
        let exit = self.eager_exit(node, DeoptReason::WrongShape);
        let done = self.ops.new_dynamic_label();
        let view = self.view;
        if value.is_some() {
            // Every successful static path proves an eligible receiver shape
            // before its field/effect. Prototype writes additionally need the
            // canonical invalidation owner and cannot commit this direct hit.
            self.emit_object_receiver(receiver, exit);
            self.emit_shape_state(receiver);
            dynasm!(self.ops ; .arch x64 ; test r10b, ShapeState::PROTOTYPE_MASK as i8 ; jnz =>exit);
        }
        for program in programs.iter() {
            let next = self.ops.new_dynamic_label();
            let object = |operand| if operand == 0 { receiver } else { holder };
            let mut committed = false;
            for op in program.ops.iter() {
                match *op {
                    Op::LoadIntrinsicPrototype { target, .. } if value.is_none() => self
                        .emit_intrinsic_prototype(
                            target,
                            pc,
                            receiver,
                            holder,
                            next,
                            abi::STUB_JIT_LOAD_PROPERTY.id,
                        ),
                    Op::LoadPrototypeHolder { root, .. } => {
                        self.emit_prototype_holder(receiver, holder, root, next)
                    }
                    Op::GuardPrototypeValidity { validity } => {
                        self.emit_validity_guard(validity, next)
                    }
                    Op::GuardShape {
                        object: operand,
                        shape,
                    } => {
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
                        if !field.is_inline() {
                            self.load_immediate(11, 0xffff_ffff_0000_0000);
                            dynasm!(self.ops ; .arch x64 ; and r11, Rq(receiver)
                                ; mov r10d, [Rq(receiver) + view.field_layout.slab_handle_byte as i32]
                                ; test r10d, r10d ; jz =>next ; add r10, r11
                                ; cmp DWORD [r10 + view.field_layout.slab_capacity_byte as i32], field.index() as i32 ; jbe =>next);
                        }
                        self.emit_shape_state(receiver);
                        dynasm!(self.ops ; .arch x64 ; test r10b, ShapeState::EXTENSIBLE_MASK as i8 ; jz =>next);
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
                            false,
                        );
                        committed = true;
                        dynasm!(self.ops ; .arch x64 ; jmp =>done);
                    }
                    Op::StoreField { field, .. } if value.is_some() => {
                        crate::x86_64::fields::emit_own_field(
                            &mut self.ops,
                            view.field_layout,
                            receiver,
                            value.unwrap(),
                            field,
                            true,
                        );
                        committed = true;
                    }
                    Op::PublishShape { shape, .. } if value.is_some() => {
                        dynasm!(self.ops ; .arch x64 ; mov DWORD [Rq(receiver) + view.object_shape_byte as i32], shape as i32);
                        self.emit_shape_child_barrier(node, receiver, shape, scratch);
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
            dynasm!(self.ops ; .arch x64 ; jmp =>done ; =>next);
        }
        dynasm!(self.ops ; .arch x64 ; jmp =>exit ; =>done);
        Ok(())
    }

    /// An ordinary array's or a string's `length` as a tagged int32, then a
    /// jump to the returned label, bound after the node; any other receiver,
    /// or a length past int32, falls through to the property load emitted
    /// after it.
    fn emit_exotic_length(&mut self, node: NodeId, receiver: u8, destination: u8) -> DynamicLabel {
        let view = self.view_of(node);
        let array_tag = view.array_layout.type_tag as i8;
        let array_length_byte = view.array_layout.length_byte as i32;
        let string_tag = view.string_layout.string_type_tag as i8;
        let string_length_byte = view.string_layout.string_len_byte as i32;
        let (string, boxed, other, done) = (
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
            ; mov r10, QWORD [Rq(receiver) + array_length_byte]
            ; jmp =>boxed
            ; =>string
            ; cmp BYTE [Rq(receiver)], string_tag
            ; jne =>other
            ; mov r10d, DWORD [Rq(receiver) + string_length_byte]
            ; =>boxed
            // A length past int32 is a Number the property load boxes.
            ; cmp r10, i32::MAX
            ; ja =>other
        );
        crate::x86_64::values::emit_box_int32(&mut self.ops, 10, destination, 11);
        dynasm!(self.ops ; .arch x64 ; jmp =>done ; =>other);
        done
    }

    fn emit_load_property_cached(
        &mut self,
        node: NodeId,
        pc: u32,
        atom: Option<u32>,
        receiver: u8,
        temps: [u8; 4],
        destination: u8,
    ) -> Result<(), Unsupported> {
        let slow = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        let table = self.ops.new_dynamic_label();
        // V8's generic LoadIC: the site's own handlers first, then the
        // isolate's shared table, then the committed miss.
        if let Some((slot, function_id, byte_pc)) = self.property_ic_slot(node, pc) {
            let view = self.view_of(node);
            emit_load_symbol_u64(
                &mut self.ops,
                &mut self.relocations,
                temps[0],
                slot,
                RelocationTarget::PropertyIcSlot {
                    function_id,
                    byte_pc,
                },
            );
            crate::x86_64::property_ic::emit_slot_load(
                &mut self.ops,
                view,
                receiver,
                temps[0],
                [temps[1], temps[2], temps[3]],
                destination,
                table,
                table,
                done,
            );
        }
        dynasm!(self.ops ; .arch x64 ; =>table);
        let cache = self.view_of(node).property_action_cache;
        emit_action_probe(
            &mut self.ops,
            &mut self.relocations,
            self.view,
            cache,
            atom.map(crate::x86_64::property_actions::AtomOperand::Immediate),
            PropertySourceAccess::Load,
            receiver,
            None,
            temps,
            Some(destination),
            slow,
            done,
            done,
        );
        dynasm!(self.ops ; .arch x64 ; =>slow);
        self.emit_property_runtime(
            node,
            pc,
            PropertySourceAccess::Load,
            [receiver, receiver],
            Some(destination),
        )?;
        dynasm!(self.ops ; .arch x64 ; =>done);
        Ok(())
    }

    fn emit_store_property_cached(
        &mut self,
        node: NodeId,
        pc: u32,
        atom: Option<u32>,
        [receiver, value]: [u8; 2],
        temps: [u8; 4],
    ) -> Result<(), Unsupported> {
        let slow = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        let appended = self.ops.new_dynamic_label();
        let table = self.ops.new_dynamic_label();
        let slot_stored = self.ops.new_dynamic_label();
        // V8's generic StoreIC: the site's own handlers, then the shared
        // table; an append leaves its compressed child in `temps[1]`.
        let slot = self.property_ic_slot(node, pc);
        if let Some((slot, function_id, byte_pc)) = slot {
            let view = self.view_of(node);
            emit_load_symbol_u64(
                &mut self.ops,
                &mut self.relocations,
                temps[0],
                slot,
                RelocationTarget::PropertyIcSlot {
                    function_id,
                    byte_pc,
                },
            );
            crate::x86_64::property_ic::emit_slot_store(
                &mut self.ops,
                view,
                receiver,
                value,
                temps[0],
                [temps[2], temps[3], temps[1]],
                table,
                table,
                slot_stored,
            );
        }
        dynasm!(self.ops ; .arch x64 ; =>table);
        let cache = self.view_of(node).property_action_cache;
        emit_action_probe(
            &mut self.ops,
            &mut self.relocations,
            self.view,
            cache,
            atom.map(crate::x86_64::property_actions::AtomOperand::Immediate),
            PropertySourceAccess::Store,
            receiver,
            Some(value),
            temps,
            None,
            slow,
            done,
            appended,
        );
        if slot.is_some() {
            dynasm!(self.ops ; .arch x64
                ; jmp =>slow
                ; =>slot_stored
                ; test Rd(temps[1]), Rd(temps[1])
                ; jz =>done);
        }
        dynasm!(self.ops ; .arch x64 ; =>appended);
        self.emit_dynamic_shape_child_barrier(node, receiver, temps[1], temps[0]);
        dynasm!(self.ops ; .arch x64 ; jmp =>done ; =>slow);
        self.emit_property_runtime(
            node,
            pc,
            PropertySourceAccess::Store,
            [receiver, value],
            None,
        )?;
        dynasm!(self.ops ; .arch x64 ; =>done);
        // The existing StorePropertyCached value-barrier node follows both
        // native actions. Child publication uses its existing NoAlloc owner.
        Ok(())
    }

    /// The native IC slot of the property site at `pc` of the body `node`
    /// belongs to: `(address, function id, byte pc)`. A site that was already
    /// megamorphic at compile time has none: that state is terminal, so its
    /// handlers stay empty and the shared table serves every receiver.
    fn property_ic_slot(&self, node: NodeId, pc: u32) -> Option<(u64, u32, u32)> {
        let view = self.view_of(node);
        let byte_pc = view.instructions.get(pc as usize)?.byte_pc;
        let access = view.property_accesses.get(&byte_pc)?;
        (access.ic_slot != 0 && !access.shared && view.cage_base != 0).then_some((
            access.ic_slot,
            view.code_block.id,
            byte_pc,
        ))
    }

    fn emit_property_runtime(
        &mut self,
        node: NodeId,
        pc: u32,
        access: PropertySourceAccess,
        [receiver, value]: [u8; 2],
        destination: Option<u8>,
    ) -> Result<(), Unsupported> {
        let view = self.view_of(node);
        let function_id = view.code_block.id;
        let byte_pc = view
            .instructions
            .get(pc as usize)
            .map(|instruction| instruction.byte_pc)
            .ok_or(Unsupported::OperandShape("x86 graph property site pc"))?;
        let slot = view
            .property_accesses
            .get(&byte_pc)
            .map(|access| access.ic_slot)
            .filter(|&slot| slot != 0)
            .ok_or(Unsupported::OperandShape(
                "x86 graph property site without an IC slot",
            ))?;
        let stub = match access {
            PropertySourceAccess::Load => abi::STUB_JIT_LOAD_PROPERTY,
            PropertySourceAccess::Store => abi::STUB_JIT_STORE_PROPERTY,
        };
        let mut arguments: smallvec::SmallVec<[CommittedArgument; 3]> =
            smallvec::smallvec![CommittedArgument::Value(receiver)];
        if matches!(access, PropertySourceAccess::Store) {
            arguments.push(CommittedArgument::Value(value));
        }
        arguments.push(CommittedArgument::Address(
            slot,
            RelocationTarget::PropertyIcSlot {
                function_id,
                byte_pc,
            },
        ));
        self.emit_committed_call(node, stub, &arguments, destination)
    }

    fn emit_instanceof(
        &mut self,
        node: NodeId,
        [value, target]: [u8; 2],
        [cage, rare, prototype, cursor, budget]: [u8; 5],
        destination: u8,
    ) -> Result<(), Unsupported> {
        let view = self.view;
        let layout = view.closure_call_layout;
        let miss = self.ops.new_dynamic_label();
        let yes = self.ops.new_dynamic_label();
        let no = self.ops.new_dynamic_label();
        let done = self.ops.new_dynamic_label();
        let symbols_absent = self.ops.new_dynamic_label();
        let walk = self.ops.new_dynamic_label();
        let step = self.ops.new_dynamic_label();
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
            self.emit_cell_guard(target, miss);
            dynasm!(self.ops ; .arch x64
                ; mov Rd(rare), Rd(target) ; add Rq(rare), Rq(cage)
                ; cmp BYTE [Rq(rare)], otter_vm::closure::JS_CLOSURE_BODY_TYPE_TAG as i8 ; jne =>miss
                ; movzx r10d, BYTE [Rq(rare) + otter_vm::closure::CLOSURE_NAMED_LOOKUP_BYTE as i32]
                ; and r10d, !u32::from(otter_vm::closure::CLOSURE_LOOKUP_OWN_PROPS) as i32
                ; cmp r10d, otter_vm::closure::CLOSURE_LOOKUP_ORDINARY as i32 ; jne =>miss
                ; mov Rd(rare), [Rq(rare) + layout.rare_byte as i32] ; test Rd(rare), Rd(rare) ; jz =>miss
                ; add Rq(rare), Rq(cage)
                ; mov r10d, [Rq(rare) + layout.own_props_byte as i32] ; test r10d, r10d ; jz =>symbols_absent
                ; add r10, Rq(cage) ; mov r10d, [r10 + view.object_exotic_handle_byte as i32]
                ; test r10d, r10d ; jz =>symbols_absent ; add r10, Rq(cage)
                ; cmp DWORD [r10 + otter_vm::object::EXOTIC_SLOTS_SYMBOL_PROPS_BYTE as i32], 0 ; jne =>miss
                ; =>symbols_absent ; mov Rq(prototype), [Rq(rare) + layout.prototype_byte as i32]);
            self.emit_cell_guard(prototype, miss);
            dynasm!(self.ops ; .arch x64 ; mov Rd(prototype), Rd(prototype) ; add Rq(prototype), Rq(cage)
                ; cmp BYTE [Rq(prototype)], crate::entry::OBJECT_BODY_TYPE_TAG as i8 ; jne =>miss);
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
        dynasm!(self.ops ; .arch x64 ; =>yes);
        self.load_immediate(destination, tag::VALUE_TRUE);
        dynasm!(self.ops ; .arch x64 ; jmp =>done ; =>no);
        self.load_immediate(destination, tag::VALUE_FALSE);
        dynasm!(self.ops ; .arch x64 ; jmp =>done ; =>miss);
        // A native target (a builtin constructor) answers through the leaf
        // probe without running JavaScript; any other case completes in the
        // runtime. The probe keeps every live value and both operands.
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
            ],
            Some(destination),
        )?;
        dynasm!(self.ops ; .arch x64 ; =>done);
        Ok(())
    }
}
