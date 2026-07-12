//! Backend-neutral template operations over typed baseline lowering.
//!
//! # Contents
//! - [`TemplateOp`] — the complete machine-independent operation set every
//!   template backend consumes.
//! - [`TemplateInstr`] — one operation bound to its canonical instruction PC.
//! - [`TemplatePlan`] — the validated linear operation stream for one
//!   function.
//!
//! # Invariants
//! - Built strictly on top of the shared typed lowering pass: operand decoding,
//!   duplicate-PC detection, and branch-target verification happen exactly once
//!   before any backend opens an assembler.
//! - An opcode outside the supported subset rejects the whole compilation with
//!   [`Unsupported::Opcode`]; a plan never describes a partially compilable
//!   function.
//! - Branch targets are canonical instruction indices already proven to name
//!   instruction boundaries; back edges are classified here so every backend
//!   places its cooperative poll identically.
//! - Immediate operands carry final boxed `Value` bit patterns; backends
//!   materialize them without consulting the constant pool.
//!
//! # See also
//! - [`super::arm64`] — the first machine-code consumer of these operations.

use otter_bytecode::Op;
use otter_vm::{JitCompileSnapshot, SafepointId, SafepointRecord, Value};

use crate::baseline::{
    BaselinePlan, MAX_METHOD_ARGS, Unsupported, pack_method_arg_regs, value_tag,
};

/// Numeric binary operators lowered to the shared int32/double template.
/// `+` has its own operation ([`TemplateOp::AddGeneric`]) because its
/// non-numeric semantics are additive, not a side exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArithKind {
    Sub,
    Mul,
    Div,
    Rem,
}

/// Comparison operators lowered to the shared int32/double template.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompareKind {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
}

/// Int32 bitwise/shift operators sharing the `ToInt32` fast path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BitwiseKind {
    Or,
    And,
    Xor,
    Shl,
    Shr,
}

/// One machine-independent template operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TemplateOp {
    /// Store the boxed `Value` bit pattern into frame register `dst`.
    LoadImmediate { dst: u16, bits: u64 },
    /// Copy frame register `src` into frame register `dst`.
    Move { dst: u16, src: u16 },
    /// Unconditional branch to the canonical instruction PC `target`.
    Jump { target: u32, back_edge: bool },
    /// Branch to `target` when `ToBoolean(r<condition>)` matches
    /// `when_truthy`; fall through otherwise.
    Branch {
        condition: u16,
        target: u32,
        when_truthy: bool,
        back_edge: bool,
    },
    /// `r<dst> = ToBoolean(r<src>)`, inverted when `negate` is set.
    Truthiness { dst: u16, src: u16, negate: bool },
    /// `r<dst> = r<lhs> <op> r<rhs>` over tagged numbers: int32 fast path with
    /// overflow promotion to double, full double path, exact side exit on a
    /// non-number operand.
    BinaryArith {
        dst: u16,
        lhs: u16,
        rhs: u16,
        kind: ArithKind,
    },
    /// `r<dst> = r<lhs> <cmp> r<rhs>` producing a boolean. Strict (in)equality
    /// additionally decides non-number immediates by raw identity; heap cells
    /// side-exit to the interpreter, which owns content equality.
    Compare {
        dst: u16,
        lhs: u16,
        rhs: u16,
        kind: CompareKind,
    },
    /// Abstract (in)equality over numbers and the null/undefined equivalence
    /// class; every coercive case takes an exact side exit.
    LooseCompare {
        dst: u16,
        lhs: u16,
        rhs: u16,
        negate: bool,
    },
    /// Int32 bitwise/shift with the full finite-double `ToInt32` fast path.
    IntBitwise {
        dst: u16,
        lhs: u16,
        rhs: u16,
        kind: BitwiseKind,
    },
    /// `r<dst> = r<lhs> >>> r<rhs>` with the full finite-double `ToUint32`
    /// fast path; the result boxes as a double (uint32-valued Number).
    UnsignedShiftRight { dst: u16, lhs: u16, rhs: u16 },
    /// `r<dst> = ToNumeric(r<src>) + delta` (update expressions).
    Increment { dst: u16, src: u16, delta: i32 },
    /// `r<dst> = -ToNumeric(r<src>)` including the `-0` and `-i32::MIN`
    /// double promotions.
    Negate { dst: u16, src: u16 },
    /// `r<dst> = ToNumeric(r<src>)` — identity on numbers, side exit
    /// otherwise.
    ToNumeric { dst: u16, src: u16 },
    /// `r<dst> = ToPrimitive(r<src>)` — identity on primitives, side exit on
    /// heap cells and function references so observable coercion hooks run in
    /// the interpreter.
    ToPrimitive { dst: u16, src: u16 },
    /// `r<dst> = r<lhs> + r<rhs>` with the full `+` semantics: inline numeric
    /// paths, an allocating string-concat runtime call rooted through the
    /// published frame at `concat_safepoint`, and the interpreter-completing
    /// delegate for every remaining coercive case.
    AddGeneric {
        dst: u16,
        lhs: u16,
        rhs: u16,
        concat_safepoint: SafepointId,
    },
    /// `r<dst>` = this-binding read from the entry context; a derived-ctor
    /// hole takes an exact side exit.
    LoadThis { dst: u16 },
    /// `r<dst>` = the running function's SELF closure bits from the entry
    /// context (named-function self binding).
    LoadSelfClosure { dst: u16 },
    /// `r<dst>` = materialized function object for `constants[constant]`.
    MakeFunction { dst: u16, constant: u32 },
    /// `r<dst>` = closure over `function` capturing `parents` upvalues.
    MakeClosure {
        dst: u16,
        function: u32,
        parents: TemplateTail,
    },
    /// `r<dst> = constants[constant]` (string constant).
    LoadString { dst: u16, constant: u32 },
    /// `r<dst> = global[name]` or throw.
    LoadGlobal { dst: u16, name: u32 },
    /// `r<dst>` = builtin error constructor for `constant`.
    LoadBuiltinError { dst: u16, constant: u32 },
    /// `r<dst> = {}`.
    NewObject { dst: u16 },
    /// `r<dst> = [elements…]` from the plan-owned register tail.
    NewArray { dst: u16, elements: TemplateTail },
    /// `r<dst> = Math.<method>(arguments…)`.
    MathCall {
        dst: u16,
        method: u32,
        arguments: TemplateTail,
    },
    /// Refresh the captured binding cell at `index` (per-iteration bindings).
    FreshUpvalue { index: i32 },
    /// `object[key] = value` as a data property definition.
    DefineDataProperty { object: u16, key: u16, value: u16 },
    /// `DefineOwnProperty(target, key, descriptor)`.
    DefineOwnProperty {
        target: u16,
        key: u16,
        descriptor: u16,
    },
    /// `r<dst> = r<receiver>[r<index>]`.
    LoadElement { dst: u16, receiver: u16, index: u16 },
    /// `r<receiver>[r<index>] = r<value>` (`scratch` is the bytecode scratch
    /// slot the runtime operation owns).
    StoreElement {
        receiver: u16,
        index: u16,
        value: u16,
        scratch: u16,
    },
    /// `r<dst> = upvalue[index]` (captured binding; TDZ raises in the VM).
    LoadUpvalue { dst: u16, index: i32 },
    /// `upvalue[index] = r<src>` (barriered store in the VM).
    StoreUpvalue { src: u16, index: i32 },
    /// `upvalue[index] = r<src>` with the TDZ read guard.
    StoreUpvalueChecked { src: u16, index: i32 },
    /// `r<dst> = r<object>.name` through the inline WhiskerIC probe and the
    /// window transition.
    LoadProperty {
        dst: u16,
        object: u16,
        name: u32,
        site: u64,
        array_length: bool,
    },
    /// `r<object>.name = r<value>` through the inline WhiskerIC probe and the
    /// window transition.
    StoreProperty {
        object: u16,
        name: u32,
        value: u16,
        site: u64,
    },
    /// `r<dst> = r<callee>(args…)` through the direct-call prepare
    /// transition; ineligible callees take an exact side exit to normal
    /// dispatch. Argument register indices are packed one per 16-bit lane.
    Call {
        dst: u16,
        callee: u16,
        argc: u16,
        packed_args: u64,
    },
    /// `r<dst> = r<receiver>.name(args…)` through the collection-method IC
    /// and the direct-method prepare transition.
    MethodCall {
        dst: u16,
        receiver: u16,
        name: u32,
        site: u64,
        argc: u16,
        packed_args: u64,
    },
    /// Return `r<src>` as the completion value.
    Return { src: u16 },
    /// Return `undefined` as the completion value.
    ReturnUndefined,
}

/// Range into a plan-owned homogeneous operand side buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TemplateTail {
    pub(crate) start: usize,
    pub(crate) len: usize,
}

/// One template operation at its canonical instruction PC.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TemplateInstr {
    pub(crate) pc: u32,
    pub(crate) op: TemplateOp,
}

/// Backend-neutral facts established before template emission starts.
///
/// The boxed operand buffers are address-stable: emitted variadic operations
/// bake pointers into them, and the finalized code object takes ownership of
/// the same allocations.
pub(crate) struct TemplatePlan {
    pub(crate) instructions: Vec<TemplateInstr>,
    pub(crate) register_count: u16,
    pub(crate) register_operands: Box<[u16]>,
    pub(crate) index_operands: Box<[u32]>,
    pub(crate) safepoint_records: Vec<SafepointRecord>,
    pub(crate) load_property_count: usize,
    pub(crate) store_property_count: usize,
}

impl TemplatePlan {
    pub(crate) fn register_tail(&self, tail: TemplateTail) -> &[u16] {
        &self.register_operands[tail.start..tail.start + tail.len]
    }

    pub(crate) fn index_tail(&self, tail: TemplateTail) -> &[u32] {
        &self.index_operands[tail.start..tail.start + tail.len]
    }
}

impl TemplatePlan {
    pub(crate) fn build(view: &JitCompileSnapshot) -> Result<Self, Unsupported> {
        let lowering = BaselinePlan::build(view)?;
        let mut instructions = Vec::with_capacity(lowering.instructions.len());
        let mut register_operands: Vec<u16> = Vec::new();
        let mut index_operands: Vec<u32> = Vec::new();
        for (meta, lowered) in view.instructions.iter().zip(&lowering.instructions) {
            let pc = lowered.instruction_pc;
            let op = match lowered.op {
                Op::LoadInt32 => {
                    let operands = lowered.load_int32_operands()?;
                    TemplateOp::LoadImmediate {
                        dst: operands.dst,
                        bits: value_tag::box_int32(operands.value),
                    }
                }
                Op::LoadNumber => {
                    let dst = lowered.destination_operands()?.dst;
                    let value = meta
                        .load_number
                        .ok_or(Unsupported::OperandShape("load-number constant"))?;
                    TemplateOp::LoadImmediate {
                        dst,
                        bits: Value::number_f64(value).to_bits(),
                    }
                }
                Op::LoadUndefined => TemplateOp::LoadImmediate {
                    dst: lowered.destination_operands()?.dst,
                    bits: value_tag::VALUE_UNDEFINED,
                },
                Op::LoadNull => TemplateOp::LoadImmediate {
                    dst: lowered.destination_operands()?.dst,
                    bits: value_tag::VALUE_NULL,
                },
                Op::LoadTrue => TemplateOp::LoadImmediate {
                    dst: lowered.destination_operands()?.dst,
                    bits: value_tag::VALUE_TRUE,
                },
                Op::LoadFalse => TemplateOp::LoadImmediate {
                    dst: lowered.destination_operands()?.dst,
                    bits: value_tag::VALUE_FALSE,
                },
                Op::LoadHole => TemplateOp::LoadImmediate {
                    dst: lowered.destination_operands()?.dst,
                    bits: value_tag::VALUE_HOLE,
                },
                Op::LoadLocal => {
                    let operands = lowered.local_operands()?;
                    TemplateOp::Move {
                        dst: operands.value,
                        src: operands.local,
                    }
                }
                Op::StoreLocal => {
                    let operands = lowered.local_operands()?;
                    TemplateOp::Move {
                        dst: operands.local,
                        src: operands.value,
                    }
                }
                Op::Jump => {
                    let target = lowered.branch_operands()?.target;
                    TemplateOp::Jump {
                        target,
                        back_edge: target <= pc,
                    }
                }
                Op::JumpIfTrue | Op::JumpIfFalse => {
                    let operands = lowered.conditional_branch_operands()?;
                    TemplateOp::Branch {
                        condition: operands.condition,
                        target: operands.target,
                        when_truthy: lowered.op == Op::JumpIfTrue,
                        back_edge: operands.target <= pc,
                    }
                }
                Op::ToBoolean => {
                    let operands = lowered.unary_operands()?;
                    TemplateOp::Truthiness {
                        dst: operands.dst,
                        src: operands.src,
                        negate: false,
                    }
                }
                Op::LogicalNot => {
                    let operands = lowered.unary_operands()?;
                    TemplateOp::Truthiness {
                        dst: operands.dst,
                        src: operands.src,
                        negate: true,
                    }
                }
                Op::Add => {
                    let operands = lowered.binary_operands()?;
                    let concat_safepoint = lowering
                        .add_alloc_safepoints
                        .get(&lowered.byte_pc)
                        .copied()
                        .ok_or(Unsupported::OperandShape("Add without a safepoint"))?;
                    TemplateOp::AddGeneric {
                        dst: operands.dst,
                        lhs: operands.lhs,
                        rhs: operands.rhs,
                        concat_safepoint,
                    }
                }
                Op::Sub | Op::Mul | Op::Div | Op::Rem => {
                    let operands = lowered.binary_operands()?;
                    TemplateOp::BinaryArith {
                        dst: operands.dst,
                        lhs: operands.lhs,
                        rhs: operands.rhs,
                        kind: match lowered.op {
                            Op::Sub => ArithKind::Sub,
                            Op::Mul => ArithKind::Mul,
                            Op::Div => ArithKind::Div,
                            _ => ArithKind::Rem,
                        },
                    }
                }
                Op::MakeFunction | Op::MakeClosure if meta.make_self => {
                    TemplateOp::LoadSelfClosure {
                        dst: lowered.destination_operands()?.dst,
                    }
                }
                Op::MakeFunction => {
                    let operands = lowered.constant_operands()?;
                    TemplateOp::MakeFunction {
                        dst: operands.dst,
                        constant: operands.constant,
                    }
                }
                Op::MakeClosure => {
                    let operands = lowered.make_closure_operands()?;
                    let slice = lowering.index_tail(operands.parents)?;
                    let start = index_operands.len();
                    index_operands.extend_from_slice(slice);
                    TemplateOp::MakeClosure {
                        dst: operands.dst,
                        function: operands.function,
                        parents: TemplateTail {
                            start,
                            len: slice.len(),
                        },
                    }
                }
                Op::LoadThis => TemplateOp::LoadThis {
                    dst: lowered.destination_operands()?.dst,
                },
                Op::LoadString => {
                    let operands = lowered.constant_operands()?;
                    TemplateOp::LoadString {
                        dst: operands.dst,
                        constant: operands.constant,
                    }
                }
                Op::LoadGlobalOrThrow => {
                    let operands = lowered.constant_operands()?;
                    TemplateOp::LoadGlobal {
                        dst: operands.dst,
                        name: operands.constant,
                    }
                }
                Op::LoadBuiltinError => {
                    let operands = lowered.constant_operands()?;
                    TemplateOp::LoadBuiltinError {
                        dst: operands.dst,
                        constant: operands.constant,
                    }
                }
                Op::NewObject => TemplateOp::NewObject {
                    dst: lowered.destination_operands()?.dst,
                },
                Op::NewArray => {
                    let operands = lowered.new_array_operands()?;
                    let slice = lowering.register_tail(operands.elements)?;
                    let start = register_operands.len();
                    register_operands.extend_from_slice(slice);
                    TemplateOp::NewArray {
                        dst: operands.dst,
                        elements: TemplateTail {
                            start,
                            len: slice.len(),
                        },
                    }
                }
                Op::MathCall => {
                    let operands = lowered.math_call_operands()?;
                    let slice = lowering.register_tail(operands.arguments)?;
                    let start = register_operands.len();
                    register_operands.extend_from_slice(slice);
                    TemplateOp::MathCall {
                        dst: operands.dst,
                        method: operands.method,
                        arguments: TemplateTail {
                            start,
                            len: slice.len(),
                        },
                    }
                }
                Op::FreshUpvalue => TemplateOp::FreshUpvalue {
                    index: lowered.immediate_operands()?.value,
                },
                Op::DefineDataProperty => {
                    let operands = lowered.triple_operands()?;
                    TemplateOp::DefineDataProperty {
                        object: operands.first,
                        key: operands.second,
                        value: operands.third,
                    }
                }
                Op::DefineOwnProperty => {
                    let operands = lowered.triple_operands()?;
                    TemplateOp::DefineOwnProperty {
                        target: operands.first,
                        key: operands.second,
                        descriptor: operands.third,
                    }
                }
                Op::LoadElement => {
                    let operands = lowered.element_load_operands()?;
                    TemplateOp::LoadElement {
                        dst: operands.dst,
                        receiver: operands.receiver,
                        index: operands.index,
                    }
                }
                Op::StoreElement => {
                    let operands = lowered.element_store_operands()?;
                    TemplateOp::StoreElement {
                        receiver: operands.receiver,
                        index: operands.index,
                        value: operands.value,
                        scratch: operands.scratch,
                    }
                }
                Op::LoadUpvalue => {
                    let operands = lowered.upvalue_operands()?;
                    TemplateOp::LoadUpvalue {
                        dst: operands.value,
                        index: operands.index,
                    }
                }
                Op::StoreUpvalue => {
                    let operands = lowered.upvalue_operands()?;
                    TemplateOp::StoreUpvalue {
                        src: operands.value,
                        index: operands.index,
                    }
                }
                Op::StoreUpvalueChecked => {
                    let operands = lowered.upvalue_operands()?;
                    TemplateOp::StoreUpvalueChecked {
                        src: operands.value,
                        index: operands.index,
                    }
                }
                Op::LoadProperty => {
                    let operands = lowered.property_load_operands()?;
                    let site = meta
                        .property_ic_site(view.code_block.as_ref())
                        .unwrap_or(usize::MAX) as u64;
                    TemplateOp::LoadProperty {
                        dst: operands.dst,
                        object: operands.object,
                        name: operands.name,
                        site,
                        array_length: meta.load_array_length,
                    }
                }
                Op::StoreProperty => {
                    let operands = lowered.property_store_operands()?;
                    let site = meta
                        .property_ic_site(view.code_block.as_ref())
                        .unwrap_or(usize::MAX) as u64;
                    TemplateOp::StoreProperty {
                        object: operands.object,
                        name: operands.name,
                        value: operands.value,
                        site,
                    }
                }
                Op::Call => {
                    let operands = lowered.call_operands()?;
                    let arguments = lowering.register_tail(operands.arguments)?;
                    if arguments.len() > MAX_METHOD_ARGS {
                        return Err(Unsupported::ArgCount(arguments.len()));
                    }
                    TemplateOp::Call {
                        dst: operands.dst,
                        callee: operands.callee,
                        argc: arguments.len() as u16,
                        packed_args: pack_method_arg_regs(arguments),
                    }
                }
                Op::CallMethodValue => {
                    let operands = lowered.method_call_operands()?;
                    let arguments = lowering.register_tail(operands.arguments)?;
                    if arguments.len() > MAX_METHOD_ARGS {
                        return Err(Unsupported::ArgCount(arguments.len()));
                    }
                    let site = meta
                        .property_ic_site(view.code_block.as_ref())
                        .unwrap_or(usize::MAX) as u64;
                    TemplateOp::MethodCall {
                        dst: operands.dst,
                        receiver: operands.receiver,
                        name: operands.name,
                        site,
                        argc: arguments.len() as u16,
                        packed_args: pack_method_arg_regs(arguments),
                    }
                }
                Op::LessThan
                | Op::LessEq
                | Op::GreaterThan
                | Op::GreaterEq
                | Op::Equal
                | Op::NotEqual => {
                    let operands = lowered.binary_operands()?;
                    TemplateOp::Compare {
                        dst: operands.dst,
                        lhs: operands.lhs,
                        rhs: operands.rhs,
                        kind: match lowered.op {
                            Op::LessThan => CompareKind::Lt,
                            Op::LessEq => CompareKind::Le,
                            Op::GreaterThan => CompareKind::Gt,
                            Op::GreaterEq => CompareKind::Ge,
                            Op::Equal => CompareKind::Eq,
                            _ => CompareKind::Ne,
                        },
                    }
                }
                Op::LooseEqual | Op::LooseNotEqual => {
                    let operands = lowered.binary_operands()?;
                    TemplateOp::LooseCompare {
                        dst: operands.dst,
                        lhs: operands.lhs,
                        rhs: operands.rhs,
                        negate: lowered.op == Op::LooseNotEqual,
                    }
                }
                Op::BitwiseOr | Op::BitwiseAnd | Op::BitwiseXor | Op::Shl | Op::Shr => {
                    let operands = lowered.binary_operands()?;
                    TemplateOp::IntBitwise {
                        dst: operands.dst,
                        lhs: operands.lhs,
                        rhs: operands.rhs,
                        kind: match lowered.op {
                            Op::BitwiseOr => BitwiseKind::Or,
                            Op::BitwiseAnd => BitwiseKind::And,
                            Op::BitwiseXor => BitwiseKind::Xor,
                            Op::Shl => BitwiseKind::Shl,
                            _ => BitwiseKind::Shr,
                        },
                    }
                }
                Op::Ushr => {
                    let operands = lowered.binary_operands()?;
                    TemplateOp::UnsignedShiftRight {
                        dst: operands.dst,
                        lhs: operands.lhs,
                        rhs: operands.rhs,
                    }
                }
                Op::Increment => {
                    let operands = lowered.increment_operands()?;
                    TemplateOp::Increment {
                        dst: operands.dst,
                        src: operands.src,
                        delta: operands.delta,
                    }
                }
                Op::Neg => {
                    let operands = lowered.unary_operands()?;
                    TemplateOp::Negate {
                        dst: operands.dst,
                        src: operands.src,
                    }
                }
                Op::ToNumeric => {
                    let operands = lowered.unary_operands()?;
                    TemplateOp::ToNumeric {
                        dst: operands.dst,
                        src: operands.src,
                    }
                }
                Op::ToPrimitive => {
                    let operands = lowered.unary_operands()?;
                    TemplateOp::ToPrimitive {
                        dst: operands.dst,
                        src: operands.src,
                    }
                }
                Op::Return | Op::ReturnValue => TemplateOp::Return {
                    src: lowered.source_operands()?.src,
                },
                Op::ReturnUndefined => TemplateOp::ReturnUndefined,
                op => return Err(Unsupported::Opcode(op)),
            };
            instructions.push(TemplateInstr { pc, op });
        }
        Ok(Self {
            instructions,
            register_count: view.code_block.register_count,
            register_operands: register_operands.into_boxed_slice(),
            index_operands: index_operands.into_boxed_slice(),
            load_property_count: lowering.load_property_count,
            store_property_count: lowering.store_property_count,
            safepoint_records: lowering.safepoint_records,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_bytecode::Operand;
    use otter_vm::jit::JitTestInstruction;

    const STRIDE: u32 = 4;

    fn view(instrs: &[(Op, Vec<Operand>)]) -> JitCompileSnapshot {
        let instructions = instrs
            .iter()
            .enumerate()
            .map(|(idx, (op, operands))| {
                JitTestInstruction::new(*op, idx as u32, idx as u32 * STRIDE, operands.clone())
            })
            .collect();
        JitCompileSnapshot::without_feedback(0, 1, 8, instructions)
    }

    #[test]
    fn plan_maps_the_supported_subset() {
        let v = view(&[
            (
                Op::LoadInt32,
                vec![Operand::Register(0), Operand::Imm32(-7)],
            ),
            (Op::LoadTrue, vec![Operand::Register(1)]),
            (
                Op::LogicalNot,
                vec![Operand::Register(2), Operand::Register(1)],
            ),
            (
                Op::JumpIfFalse,
                vec![Operand::Imm32(1), Operand::Register(2)],
            ),
            (
                Op::StoreLocal,
                vec![Operand::Register(0), Operand::Imm32(3)],
            ),
            (Op::ReturnValue, vec![Operand::Register(0)]),
        ]);
        let plan = TemplatePlan::build(&v).expect("plan");
        assert_eq!(plan.register_count, 8);
        assert_eq!(
            plan.instructions[0].op,
            TemplateOp::LoadImmediate {
                dst: 0,
                bits: value_tag::box_int32(-7),
            }
        );
        assert_eq!(
            plan.instructions[1].op,
            TemplateOp::LoadImmediate {
                dst: 1,
                bits: value_tag::VALUE_TRUE,
            }
        );
        assert_eq!(
            plan.instructions[2].op,
            TemplateOp::Truthiness {
                dst: 2,
                src: 1,
                negate: true,
            }
        );
        assert_eq!(
            plan.instructions[3].op,
            TemplateOp::Branch {
                condition: 2,
                target: 5,
                when_truthy: false,
                back_edge: false,
            }
        );
        assert_eq!(plan.instructions[4].op, TemplateOp::Move { dst: 3, src: 0 });
        assert_eq!(plan.instructions[5].op, TemplateOp::Return { src: 0 });
    }

    #[test]
    fn plan_classifies_back_edges() {
        let v = view(&[
            (Op::LoadInt32, vec![Operand::Register(0), Operand::Imm32(1)]),
            (Op::Jump, vec![Operand::Imm32(-2)]),
            (Op::ReturnUndefined, vec![]),
        ]);
        let plan = TemplatePlan::build(&v).expect("plan");
        assert_eq!(
            plan.instructions[1].op,
            TemplateOp::Jump {
                target: 0,
                back_edge: true,
            }
        );
        assert_eq!(plan.instructions[2].op, TemplateOp::ReturnUndefined);
    }

    #[test]
    fn plan_rejects_the_whole_function_on_an_unsupported_opcode() {
        let v = view(&[
            (Op::LoadInt32, vec![Operand::Register(0), Operand::Imm32(1)]),
            (Op::Throw, vec![Operand::Register(1)]),
            (Op::ReturnValue, vec![Operand::Register(1)]),
        ]);
        assert_eq!(
            TemplatePlan::build(&v).err(),
            Some(Unsupported::Opcode(Op::Throw))
        );
    }

    #[test]
    fn plan_rejects_non_boundary_branch_targets() {
        let v = view(&[
            (Op::Jump, vec![Operand::Imm32(8)]),
            (Op::ReturnUndefined, vec![]),
        ]);
        assert_eq!(
            TemplatePlan::build(&v).err(),
            Some(Unsupported::BranchTarget(9))
        );
    }
}
