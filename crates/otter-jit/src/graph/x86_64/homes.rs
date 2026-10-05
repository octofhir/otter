//! x86 value movement and canonical home lifetime at machine boundaries.
//!
//! # Contents
//! - GP/FP/constant/slot movement and shared parallel assignment encoding.
//! - Exact-live noncollecting saves and collecting canonical-home writeback.
//! - Tagged home expiration for collecting calls and abandoned native frames.
//!
//! # Invariants
//! - Every temporary stack reservation updates the same slot-base delta.
//! - MOV-based expiration preserves pending condition flags and ABI operands.
//! - Untagged homes and the exception scratch never enter normal expiration.
//!
//! # See also
//! - `super::super::moves` owns cycle scheduling independently of this encoder.

use super::*;

impl Codegen<'_> {
    pub(super) fn slot_offset(&self, location: Location) -> i32 {
        i32::try_from(self.slots.offset(location) + self.sp_delta).expect("validated native frame")
    }
    pub(super) fn load_slot_gp(&mut self, register: u8, slot: Location) {
        let offset = self.slot_offset(slot);
        dynasm!(self.ops ; .arch x64 ; mov Rq(register), [rsp + offset]);
    }
    pub(super) fn store_slot_gp(&mut self, register: u8, slot: Location) {
        let offset = self.slot_offset(slot);
        dynasm!(self.ops ; .arch x64 ; mov [rsp + offset], Rq(register));
    }
    pub(super) fn load_slot_fp(&mut self, register: u8, slot: Location) {
        let offset = self.slot_offset(slot);
        dynasm!(self.ops ; .arch x64 ; movsd Rx(register), [rsp + offset]);
    }
    pub(super) fn store_slot_fp(&mut self, register: u8, slot: Location) {
        let offset = self.slot_offset(slot);
        dynasm!(self.ops ; .arch x64 ; movsd [rsp + offset], Rx(register));
    }
    pub(super) fn constant_bits(&self, node: NodeId) -> u64 {
        match self.graph.node(node).kind {
            Kind::ConstTagged(bits) | Kind::ConstFloat64(bits) => bits,
            Kind::ConstInt32(value) => u64::from(value as u32),
            _ => unreachable!("constant node"),
        }
    }
    pub(super) fn emit_move(&mut self, from: Location, to: Location) {
        if from == to {
            return;
        }
        match (from, to) {
            (Location::Gp(a), Location::Gp(b)) => dynasm!(self.ops ; .arch x64 ; mov Rq(b), Rq(a)),
            (Location::Fp(a), Location::Fp(b)) => {
                dynasm!(self.ops ; .arch x64 ; movsd Rx(b), Rx(a))
            }
            (Location::Gp(a), Location::Fp(b)) => dynasm!(self.ops ; .arch x64 ; movq Rx(b), Rq(a)),
            (Location::Fp(a), Location::Gp(b)) => dynasm!(self.ops ; .arch x64 ; movq Rq(b), Rx(a)),
            (Location::Gp(a), slot @ (Location::TaggedSlot(_) | Location::UntaggedSlot(_))) => {
                self.store_slot_gp(a, slot)
            }
            (Location::Fp(a), slot @ (Location::TaggedSlot(_) | Location::UntaggedSlot(_))) => {
                self.store_slot_fp(a, slot)
            }
            (slot @ (Location::TaggedSlot(_) | Location::UntaggedSlot(_)), Location::Gp(b)) => {
                self.load_slot_gp(b, slot)
            }
            (slot @ (Location::TaggedSlot(_) | Location::UntaggedSlot(_)), Location::Fp(b)) => {
                self.load_slot_fp(b, slot)
            }
            (
                from @ (Location::TaggedSlot(_) | Location::UntaggedSlot(_)),
                to @ (Location::TaggedSlot(_) | Location::UntaggedSlot(_)),
            ) => {
                self.load_slot_gp(10, from);
                self.store_slot_gp(10, to);
            }
            (Location::Constant(node), Location::Gp(b)) => {
                self.load_immediate(b, self.constant_bits(node))
            }
            (Location::Constant(node), Location::Fp(b)) => {
                self.load_immediate(10, self.constant_bits(node));
                dynasm!(self.ops ; .arch x64 ; movq Rx(b), r10);
            }
            (
                Location::Constant(node),
                slot @ (Location::TaggedSlot(_) | Location::UntaggedSlot(_)),
            ) => {
                self.load_immediate(10, self.constant_bits(node));
                self.store_slot_gp(10, slot);
            }
            (_, Location::Constant(_)) => unreachable!("a constant is never a destination"),
        }
    }
    pub(super) fn emit_parallel_moves(&mut self, moves: Vec<Move>) {
        super::super::moves::emit(self, moves);
    }
    pub(super) fn emit_park(&mut self, source: Location) {
        self.emit_move(source, Location::Gp(10));
        dynasm!(self.ops ; .arch x64 ; lea rsp, [rsp - 16] ; mov [rsp], r10);
        self.sp_delta += 16;
    }
    pub(super) fn emit_unpark(&mut self, to: Location) {
        dynasm!(self.ops ; .arch x64 ; mov r10, [rsp] ; lea rsp, [rsp + 16]);
        self.sp_delta -= 16;
        self.emit_move(Location::Gp(10), to);
    }
    pub(super) fn save_live_homes(&mut self, node: NodeId) {
        for (location, home) in self.loc(node).live_homes.clone() {
            if !matches!(home, Location::Constant(_)) {
                self.emit_move(location, home);
            }
        }
    }
    pub(super) fn restore_live_homes(&mut self, node: NodeId) {
        for (location, home) in self.loc(node).live_homes.clone() {
            self.emit_move(home, location);
        }
    }
    /// NoAlloc calls use a transient untraced save span, never a GC recipe.
    pub(super) fn emit_save_registers(&mut self, live: &[(Location, Repr)]) -> u32 {
        let bytes = (live.len() as u32 * 8).next_multiple_of(16);
        if bytes == 0 {
            return 0;
        }
        dynasm!(self.ops ; .arch x64 ; sub rsp, bytes as i32);
        self.sp_delta += bytes;
        for (index, &(location, _)) in live.iter().enumerate() {
            let offset = (index * 8) as i32;
            match location {
                Location::Gp(reg) => dynasm!(self.ops ; .arch x64 ; mov [rsp + offset], Rq(reg)),
                Location::Fp(reg) => dynasm!(self.ops ; .arch x64 ; movsd [rsp + offset], Rx(reg)),
                _ => unreachable!("live register save"),
            }
        }
        bytes
    }
    pub(super) fn emit_restore_registers(&mut self, live: &[(Location, Repr)], bytes: u32) {
        if bytes == 0 {
            return;
        }
        for (index, &(location, _)) in live.iter().enumerate() {
            let offset = (index * 8) as i32;
            match location {
                Location::Gp(reg) => dynasm!(self.ops ; .arch x64 ; mov Rq(reg), [rsp + offset]),
                Location::Fp(reg) => dynasm!(self.ops ; .arch x64 ; movsd Rx(reg), [rsp + offset]),
                _ => unreachable!("live register restore"),
            }
        }
        dynasm!(self.ops ; .arch x64 ; add rsp, bytes as i32);
        self.sp_delta -= bytes;
    }
    pub(super) fn emit_push_actuals(&mut self, inputs: &[Location]) -> Result<u32, Unsupported> {
        let bytes = crate::call_linkage::pushed_argument_bytes(inputs.len())?;
        if bytes != 0 {
            dynasm!(self.ops ; .arch x64 ; sub rsp, bytes as i32);
            self.sp_delta += bytes;
        }
        for (index, &location) in inputs.iter().enumerate() {
            self.emit_move(location, Location::Gp(10));
            dynasm!(self.ops ; .arch x64 ; mov [rsp + (index * 8) as i32], r10);
        }
        Ok(bytes)
    }
    pub(super) fn emit_pop_actuals(&mut self, bytes: u32) {
        if bytes != 0 {
            dynasm!(self.ops ; .arch x64 ; add rsp, bytes as i32);
            self.sp_delta -= bytes;
        }
    }
}

impl super::super::moves::Emitter for Codegen<'_> {
    fn move_value(&mut self, from: Location, to: Location) {
        self.emit_move(from, to);
    }
    fn park(&mut self, from: Location) {
        self.emit_park(from);
    }
    fn unpark(&mut self, to: Location) {
        self.emit_unpark(to);
    }
}
