//! Compact immutable bytecode for the new VM.

use crate::closure::UpvalueId;
use crate::frame::{FrameLayout, RegisterIndex};
use crate::property::PropertyNameId;
use crate::string::StringId;

/// Program counter type for the new VM bytecode.
pub type ProgramCounter = u32;

/// User-visible register index referenced by bytecode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct BytecodeRegister(RegisterIndex);

impl BytecodeRegister {
    /// Creates a bytecode-visible register operand.
    #[must_use]
    pub const fn new(index: RegisterIndex) -> Self {
        Self(index)
    }

    /// Returns the register index as encoded in bytecode.
    #[must_use]
    pub const fn index(self) -> RegisterIndex {
        self.0
    }

    /// Resolves the bytecode-visible register into an absolute frame register index.
    #[must_use]
    pub const fn resolve(self, layout: FrameLayout) -> Option<RegisterIndex> {
        layout.resolve_user_visible(self.0)
    }
}

/// Relative bytecode jump offset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct JumpOffset(i32);

impl JumpOffset {
    /// Creates a relative jump offset.
    #[must_use]
    pub const fn new(offset: i32) -> Self {
        Self(offset)
    }

    /// Returns the raw signed offset.
    #[must_use]
    pub const fn value(self) -> i32 {
        self.0
    }
}

/// Initial opcode set for the new VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Opcode {
    /// No operation.
    Nop = 0x00,
    /// Copy a register: `dst = src`.
    Move = 0x01,
    /// Load a 32-bit integer immediate.
    LoadI32 = 0x02,
    /// Load boolean `true`.
    LoadTrue = 0x03,
    /// Load boolean `false`.
    LoadFalse = 0x04,
    /// Allocate a plain object.
    NewObject = 0x05,
    /// Load a string literal from the current function side table.
    LoadString = 0x06,
    /// Allocate a dense array.
    NewArray = 0x07,
    /// Allocate a closure from the current function closure side table.
    NewClosure = 0x08,
    /// Load `undefined`.
    LoadUndefined = 0x09,
    /// Load `null`.
    LoadNull = 0x0A,
    /// Boolean negation.
    Not = 0x0B,
    /// Integer-or-number addition.
    Add = 0x10,
    /// Integer-or-number subtraction.
    Sub = 0x11,
    /// Integer-or-number multiplication.
    Mul = 0x12,
    /// Integer-or-number division.
    Div = 0x13,
    /// Equality comparison.
    Eq = 0x20,
    /// Less-than comparison.
    Lt = 0x21,
    /// Load a named property from an object.
    GetProperty = 0x22,
    /// Store a named property on an object.
    SetProperty = 0x23,
    /// Load an indexed element from an array or string.
    GetIndex = 0x24,
    /// Store an indexed element on an array.
    SetIndex = 0x25,
    /// Load an upvalue from the current closure context.
    GetUpvalue = 0x26,
    /// Store an upvalue on the current closure context.
    SetUpvalue = 0x27,
    /// Unconditional jump.
    Jump = 0x30,
    /// Jump if the condition is truthy.
    JumpIfTrue = 0x31,
    /// Jump if the condition is falsy.
    JumpIfFalse = 0x32,
    /// Return a register value.
    Return = 0x40,
    /// Call a direct callee with an explicit contiguous argument window.
    CallDirect = 0x41,
    /// Call a closure value with an explicit contiguous argument window.
    CallClosure = 0x42,
}

impl Opcode {
    /// Decodes an opcode byte.
    #[must_use]
    pub const fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            0x00 => Some(Self::Nop),
            0x01 => Some(Self::Move),
            0x02 => Some(Self::LoadI32),
            0x03 => Some(Self::LoadTrue),
            0x04 => Some(Self::LoadFalse),
            0x05 => Some(Self::NewObject),
            0x06 => Some(Self::LoadString),
            0x07 => Some(Self::NewArray),
            0x08 => Some(Self::NewClosure),
            0x09 => Some(Self::LoadUndefined),
            0x0A => Some(Self::LoadNull),
            0x0B => Some(Self::Not),
            0x10 => Some(Self::Add),
            0x11 => Some(Self::Sub),
            0x12 => Some(Self::Mul),
            0x13 => Some(Self::Div),
            0x20 => Some(Self::Eq),
            0x21 => Some(Self::Lt),
            0x22 => Some(Self::GetProperty),
            0x23 => Some(Self::SetProperty),
            0x24 => Some(Self::GetIndex),
            0x25 => Some(Self::SetIndex),
            0x26 => Some(Self::GetUpvalue),
            0x27 => Some(Self::SetUpvalue),
            0x30 => Some(Self::Jump),
            0x31 => Some(Self::JumpIfTrue),
            0x32 => Some(Self::JumpIfFalse),
            0x40 => Some(Self::Return),
            0x41 => Some(Self::CallDirect),
            0x42 => Some(Self::CallClosure),
            _ => None,
        }
    }
}

/// Compact 64-bit runtime instruction.
///
/// Encodings currently used:
///
/// - register/register/register:
///   `opcode:u8 | a:u16 | b:u16 | c:u16 | reserved:u8`
/// - register/immediate:
///   `opcode:u8 | a:u16 | imm32:u32 | reserved:u8`
/// - jump:
///   `opcode:u8 | imm32:i32 | reserved:24`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Instruction(u64);

impl Instruction {
    const OPCODE_SHIFT: u32 = 0;
    const A_SHIFT: u32 = 8;
    const B_SHIFT: u32 = 24;
    const C_SHIFT: u32 = 40;

    /// Encodes a no-op.
    #[must_use]
    pub const fn nop() -> Self {
        Self::from_opcode(Opcode::Nop)
    }

    /// Encodes a move instruction.
    #[must_use]
    pub const fn move_(dst: BytecodeRegister, src: BytecodeRegister) -> Self {
        Self::encode_abc(Opcode::Move, dst, src, BytecodeRegister::new(0))
    }

    /// Encodes a 32-bit integer load.
    #[must_use]
    pub const fn load_i32(dst: BytecodeRegister, value: i32) -> Self {
        Self::encode_ai32(Opcode::LoadI32, dst, value)
    }

    /// Encodes a `true` load.
    #[must_use]
    pub const fn load_true(dst: BytecodeRegister) -> Self {
        Self::encode_abc(
            Opcode::LoadTrue,
            dst,
            BytecodeRegister::new(0),
            BytecodeRegister::new(0),
        )
    }

    /// Encodes a `false` load.
    #[must_use]
    pub const fn load_false(dst: BytecodeRegister) -> Self {
        Self::encode_abc(
            Opcode::LoadFalse,
            dst,
            BytecodeRegister::new(0),
            BytecodeRegister::new(0),
        )
    }

    /// Encodes an object allocation.
    #[must_use]
    pub const fn new_object(dst: BytecodeRegister) -> Self {
        Self::encode_abc(
            Opcode::NewObject,
            dst,
            BytecodeRegister::new(0),
            BytecodeRegister::new(0),
        )
    }

    /// Encodes a string-literal load.
    #[must_use]
    pub const fn load_string(dst: BytecodeRegister, string: StringId) -> Self {
        Self::encode_abc(
            Opcode::LoadString,
            dst,
            BytecodeRegister::new(string.0),
            BytecodeRegister::new(0),
        )
    }

    /// Encodes a dense-array allocation.
    #[must_use]
    pub const fn new_array(dst: BytecodeRegister) -> Self {
        Self::encode_abc(
            Opcode::NewArray,
            dst,
            BytecodeRegister::new(0),
            BytecodeRegister::new(0),
        )
    }

    /// Encodes a closure allocation using a contiguous capture window.
    #[must_use]
    pub const fn new_closure(dst: BytecodeRegister, capture_start: BytecodeRegister) -> Self {
        Self::encode_abc(
            Opcode::NewClosure,
            dst,
            capture_start,
            BytecodeRegister::new(0),
        )
    }

    /// Encodes an `undefined` load.
    #[must_use]
    pub const fn load_undefined(dst: BytecodeRegister) -> Self {
        Self::encode_abc(
            Opcode::LoadUndefined,
            dst,
            BytecodeRegister::new(0),
            BytecodeRegister::new(0),
        )
    }

    /// Encodes a `null` load.
    #[must_use]
    pub const fn load_null(dst: BytecodeRegister) -> Self {
        Self::encode_abc(
            Opcode::LoadNull,
            dst,
            BytecodeRegister::new(0),
            BytecodeRegister::new(0),
        )
    }

    /// Encodes a boolean negation.
    #[must_use]
    pub const fn not(dst: BytecodeRegister, src: BytecodeRegister) -> Self {
        Self::encode_abc(Opcode::Not, dst, src, BytecodeRegister::new(0))
    }

    /// Encodes an addition.
    #[must_use]
    pub const fn add(dst: BytecodeRegister, lhs: BytecodeRegister, rhs: BytecodeRegister) -> Self {
        Self::encode_abc(Opcode::Add, dst, lhs, rhs)
    }

    /// Encodes a subtraction.
    #[must_use]
    pub const fn sub(dst: BytecodeRegister, lhs: BytecodeRegister, rhs: BytecodeRegister) -> Self {
        Self::encode_abc(Opcode::Sub, dst, lhs, rhs)
    }

    /// Encodes a multiplication.
    #[must_use]
    pub const fn mul(dst: BytecodeRegister, lhs: BytecodeRegister, rhs: BytecodeRegister) -> Self {
        Self::encode_abc(Opcode::Mul, dst, lhs, rhs)
    }

    /// Encodes a division.
    #[must_use]
    pub const fn div(dst: BytecodeRegister, lhs: BytecodeRegister, rhs: BytecodeRegister) -> Self {
        Self::encode_abc(Opcode::Div, dst, lhs, rhs)
    }

    /// Encodes an equality comparison.
    #[must_use]
    pub const fn eq(dst: BytecodeRegister, lhs: BytecodeRegister, rhs: BytecodeRegister) -> Self {
        Self::encode_abc(Opcode::Eq, dst, lhs, rhs)
    }

    /// Encodes a less-than comparison.
    #[must_use]
    pub const fn lt(dst: BytecodeRegister, lhs: BytecodeRegister, rhs: BytecodeRegister) -> Self {
        Self::encode_abc(Opcode::Lt, dst, lhs, rhs)
    }

    /// Encodes a named property load.
    #[must_use]
    pub const fn get_property(
        dst: BytecodeRegister,
        object: BytecodeRegister,
        property: PropertyNameId,
    ) -> Self {
        Self::encode_abc(
            Opcode::GetProperty,
            dst,
            object,
            BytecodeRegister::new(property.0),
        )
    }

    /// Encodes a named property store.
    #[must_use]
    pub const fn set_property(
        object: BytecodeRegister,
        src: BytecodeRegister,
        property: PropertyNameId,
    ) -> Self {
        Self::encode_abc(
            Opcode::SetProperty,
            object,
            src,
            BytecodeRegister::new(property.0),
        )
    }

    /// Encodes an indexed load.
    #[must_use]
    pub const fn get_index(
        dst: BytecodeRegister,
        target: BytecodeRegister,
        index: BytecodeRegister,
    ) -> Self {
        Self::encode_abc(Opcode::GetIndex, dst, target, index)
    }

    /// Encodes an indexed store.
    #[must_use]
    pub const fn set_index(
        target: BytecodeRegister,
        index: BytecodeRegister,
        src: BytecodeRegister,
    ) -> Self {
        Self::encode_abc(Opcode::SetIndex, target, index, src)
    }

    /// Encodes an upvalue load.
    #[must_use]
    pub const fn get_upvalue(dst: BytecodeRegister, upvalue: UpvalueId) -> Self {
        Self::encode_abc(
            Opcode::GetUpvalue,
            dst,
            BytecodeRegister::new(upvalue.0),
            BytecodeRegister::new(0),
        )
    }

    /// Encodes an upvalue store.
    #[must_use]
    pub const fn set_upvalue(src: BytecodeRegister, upvalue: UpvalueId) -> Self {
        Self::encode_abc(
            Opcode::SetUpvalue,
            src,
            BytecodeRegister::new(upvalue.0),
            BytecodeRegister::new(0),
        )
    }

    /// Encodes an unconditional jump.
    #[must_use]
    pub const fn jump(offset: JumpOffset) -> Self {
        Self::encode_i32_only(Opcode::Jump, offset.value())
    }

    /// Encodes a conditional jump on truthiness.
    #[must_use]
    pub const fn jump_if_true(cond: BytecodeRegister, offset: JumpOffset) -> Self {
        Self::encode_ai32(Opcode::JumpIfTrue, cond, offset.value())
    }

    /// Encodes a conditional jump on falsiness.
    #[must_use]
    pub const fn jump_if_false(cond: BytecodeRegister, offset: JumpOffset) -> Self {
        Self::encode_ai32(Opcode::JumpIfFalse, cond, offset.value())
    }

    /// Encodes a return instruction.
    #[must_use]
    pub const fn ret(src: BytecodeRegister) -> Self {
        Self::encode_abc(
            Opcode::Return,
            src,
            BytecodeRegister::new(0),
            BytecodeRegister::new(0),
        )
    }

    /// Encodes a direct call using the argument window starting at `arg_start`.
    #[must_use]
    pub const fn call_direct(dst: BytecodeRegister, arg_start: BytecodeRegister) -> Self {
        Self::encode_abc(Opcode::CallDirect, dst, arg_start, BytecodeRegister::new(0))
    }

    /// Encodes a closure call.
    #[must_use]
    pub const fn call_closure(
        dst: BytecodeRegister,
        callee: BytecodeRegister,
        arg_start: BytecodeRegister,
    ) -> Self {
        Self::encode_abc(Opcode::CallClosure, dst, callee, arg_start)
    }

    /// Returns the decoded opcode.
    #[must_use]
    pub const fn opcode(self) -> Opcode {
        let raw = ((self.0 >> Self::OPCODE_SHIFT) & 0xff) as u8;
        match Opcode::from_byte(raw) {
            Some(opcode) => opcode,
            None => panic!("invalid opcode encoding"),
        }
    }

    /// Returns the first 16-bit operand.
    #[must_use]
    pub const fn a(self) -> u16 {
        ((self.0 >> Self::A_SHIFT) & 0xffff) as u16
    }

    /// Returns the second 16-bit operand.
    #[must_use]
    pub const fn b(self) -> u16 {
        ((self.0 >> Self::B_SHIFT) & 0xffff) as u16
    }

    /// Returns the third 16-bit operand.
    #[must_use]
    pub const fn c(self) -> u16 {
        ((self.0 >> Self::C_SHIFT) & 0xffff) as u16
    }

    /// Returns the 32-bit signed immediate field.
    #[must_use]
    pub const fn immediate_i32(self) -> i32 {
        ((self.0 >> Self::B_SHIFT) & 0xffff_ffff) as u32 as i32
    }

    /// Returns the raw encoded word.
    #[must_use]
    pub const fn raw(self) -> u64 {
        self.0
    }

    #[must_use]
    const fn from_opcode(opcode: Opcode) -> Self {
        Self((opcode as u64) << Self::OPCODE_SHIFT)
    }

    #[must_use]
    const fn encode_abc(
        opcode: Opcode,
        a: BytecodeRegister,
        b: BytecodeRegister,
        c: BytecodeRegister,
    ) -> Self {
        let word = (opcode as u64) << Self::OPCODE_SHIFT
            | (a.index() as u64) << Self::A_SHIFT
            | (b.index() as u64) << Self::B_SHIFT
            | (c.index() as u64) << Self::C_SHIFT;
        Self(word)
    }

    #[must_use]
    const fn encode_ai32(opcode: Opcode, a: BytecodeRegister, immediate: i32) -> Self {
        let word = (opcode as u64) << Self::OPCODE_SHIFT
            | (a.index() as u64) << Self::A_SHIFT
            | ((immediate as u32) as u64) << Self::B_SHIFT;
        Self(word)
    }

    #[must_use]
    const fn encode_i32_only(opcode: Opcode, immediate: i32) -> Self {
        let word =
            (opcode as u64) << Self::OPCODE_SHIFT | ((immediate as u32) as u64) << Self::B_SHIFT;
        Self(word)
    }
}

/// Immutable instruction stream for the new VM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bytecode {
    instructions: Box<[Instruction]>,
}

impl Bytecode {
    /// Creates an immutable instruction stream from owned instructions.
    #[must_use]
    pub fn new(instructions: Vec<Instruction>) -> Self {
        Self {
            instructions: instructions.into_boxed_slice(),
        }
    }

    /// Creates an empty immutable instruction stream.
    #[must_use]
    pub fn empty() -> Self {
        Self::new(Vec::new())
    }

    /// Returns the number of instructions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.instructions.len()
    }

    /// Returns `true` when there are no instructions.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.instructions.is_empty()
    }

    /// Returns the instruction at the given program counter.
    #[must_use]
    pub fn get(&self, pc: ProgramCounter) -> Option<Instruction> {
        self.instructions.get(pc as usize).copied()
    }

    /// Returns the immutable instruction slice.
    #[must_use]
    pub fn instructions(&self) -> &[Instruction] {
        &self.instructions
    }
}

impl Default for Bytecode {
    fn default() -> Self {
        Self::empty()
    }
}

impl From<Vec<Instruction>> for Bytecode {
    fn from(instructions: Vec<Instruction>) -> Self {
        Self::new(instructions)
    }
}

#[cfg(test)]
mod tests {
    use crate::closure::UpvalueId;
    use crate::frame::FrameLayout;
    use crate::property::PropertyNameId;
    use crate::string::StringId;

    use super::{Bytecode, BytecodeRegister, Instruction, JumpOffset, Opcode};

    #[test]
    fn abc_instruction_round_trips() {
        let instruction = Instruction::add(
            BytecodeRegister::new(1),
            BytecodeRegister::new(2),
            BytecodeRegister::new(3),
        );

        assert_eq!(instruction.opcode(), Opcode::Add);
        assert_eq!(instruction.a(), 1);
        assert_eq!(instruction.b(), 2);
        assert_eq!(instruction.c(), 3);
    }

    #[test]
    fn immediate_instruction_round_trips() {
        let load = Instruction::load_i32(BytecodeRegister::new(4), -17);
        let jump = Instruction::jump(JumpOffset::new(-9));
        let object = Instruction::new_object(BytecodeRegister::new(6));
        let string = Instruction::load_string(BytecodeRegister::new(7), StringId(11));
        let array = Instruction::new_array(BytecodeRegister::new(8));
        let closure = Instruction::new_closure(BytecodeRegister::new(9), BytecodeRegister::new(10));
        let call = Instruction::call_direct(BytecodeRegister::new(9), BytecodeRegister::new(10));

        assert_eq!(load.opcode(), Opcode::LoadI32);
        assert_eq!(load.a(), 4);
        assert_eq!(load.immediate_i32(), -17);
        assert_eq!(object.opcode(), Opcode::NewObject);
        assert_eq!(object.a(), 6);
        assert_eq!(string.opcode(), Opcode::LoadString);
        assert_eq!(string.a(), 7);
        assert_eq!(string.b(), 11);
        assert_eq!(array.opcode(), Opcode::NewArray);
        assert_eq!(array.a(), 8);
        assert_eq!(closure.opcode(), Opcode::NewClosure);
        assert_eq!(closure.a(), 9);
        assert_eq!(closure.b(), 10);
        assert_eq!(call.opcode(), Opcode::CallDirect);
        assert_eq!(call.a(), 9);
        assert_eq!(call.b(), 10);

        assert_eq!(jump.opcode(), Opcode::Jump);
        assert_eq!(jump.immediate_i32(), -9);
    }

    #[test]
    fn property_instructions_round_trip() {
        let get = Instruction::get_property(
            BytecodeRegister::new(2),
            BytecodeRegister::new(0),
            PropertyNameId(7),
        );
        let set = Instruction::set_property(
            BytecodeRegister::new(0),
            BytecodeRegister::new(1),
            PropertyNameId(7),
        );
        let get_index = Instruction::get_index(
            BytecodeRegister::new(3),
            BytecodeRegister::new(4),
            BytecodeRegister::new(5),
        );
        let set_index = Instruction::set_index(
            BytecodeRegister::new(4),
            BytecodeRegister::new(5),
            BytecodeRegister::new(6),
        );
        let get_upvalue = Instruction::get_upvalue(BytecodeRegister::new(7), UpvalueId(8));
        let set_upvalue = Instruction::set_upvalue(BytecodeRegister::new(9), UpvalueId(10));
        let call_closure = Instruction::call_closure(
            BytecodeRegister::new(11),
            BytecodeRegister::new(12),
            BytecodeRegister::new(13),
        );

        assert_eq!(get.opcode(), Opcode::GetProperty);
        assert_eq!(get.a(), 2);
        assert_eq!(get.b(), 0);
        assert_eq!(get.c(), 7);

        assert_eq!(set.opcode(), Opcode::SetProperty);
        assert_eq!(set.a(), 0);
        assert_eq!(set.b(), 1);
        assert_eq!(set.c(), 7);

        assert_eq!(get_index.opcode(), Opcode::GetIndex);
        assert_eq!(get_index.a(), 3);
        assert_eq!(get_index.b(), 4);
        assert_eq!(get_index.c(), 5);

        assert_eq!(set_index.opcode(), Opcode::SetIndex);
        assert_eq!(set_index.a(), 4);
        assert_eq!(set_index.b(), 5);
        assert_eq!(set_index.c(), 6);

        assert_eq!(get_upvalue.opcode(), Opcode::GetUpvalue);
        assert_eq!(get_upvalue.a(), 7);
        assert_eq!(get_upvalue.b(), 8);

        assert_eq!(set_upvalue.opcode(), Opcode::SetUpvalue);
        assert_eq!(set_upvalue.a(), 9);
        assert_eq!(set_upvalue.b(), 10);

        assert_eq!(call_closure.opcode(), Opcode::CallClosure);
        assert_eq!(call_closure.a(), 11);
        assert_eq!(call_closure.b(), 12);
        assert_eq!(call_closure.c(), 13);
    }

    #[test]
    fn bytecode_register_resolves_against_frame_layout() {
        let layout = FrameLayout::new(2, 3, 4, 5).expect("layout should be valid");
        let register = BytecodeRegister::new(0);
        let last = BytecodeRegister::new(layout.user_visible_count() - 1);

        assert_eq!(register.resolve(layout), Some(layout.user_visible_start()));
        assert_eq!(last.resolve(layout), Some(layout.register_count() - 1));
        assert_eq!(
            BytecodeRegister::new(layout.user_visible_count()).resolve(layout),
            None
        );
    }

    #[test]
    fn bytecode_container_is_immutable_slice() {
        let bytecode = Bytecode::from(vec![
            Instruction::nop(),
            Instruction::ret(BytecodeRegister::new(0)),
        ]);

        assert_eq!(bytecode.len(), 2);
        assert!(!bytecode.is_empty());
        assert_eq!(bytecode.get(0).map(Instruction::opcode), Some(Opcode::Nop));
        assert_eq!(
            bytecode.get(1).map(Instruction::opcode),
            Some(Opcode::Return)
        );
        assert_eq!(bytecode.get(2), None);
    }
}
