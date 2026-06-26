//! Typed SSA graph → Cranelift IR lowering for the optimizing tier.
//!
//! Builds one Cranelift function for a graph's entry. The machine entry block
//! loads the frame register-window base from `JitCtx` and lowers the graph's
//! entry block (its `Param`s read the window); every other reachable block
//! becomes a Cranelift block, with `Phi`s as block parameters and phi inputs
//! passed as branch arguments on each predecessor edge. Speculation guards
//! branch to cold side-exit blocks that bail to the interpreter at the exact PC
//! (see [`super::deopt`]).
//!
//! # Contents
//! - [`compile_function`] — declare, lower, and define one graph as a CLIF
//!   function, returning its `FuncId` and finalized code size.
//!
//! # Invariants
//! - Only blocks the builder left with a terminator are lowered. An OSR build
//!   leaves the blocks unreachable from the loop header terminator-less; they are
//!   never targeted and are skipped, so the emitted CFG is exactly the reachable
//!   region.
//! - `Int32Add`/`Sub`/`Mul` deopt on signed-overflow, detected by widening to
//!   `i64`, reducing, and re-widening: the reduced result re-sign-extends to the
//!   wide sum iff no overflow occurred — exact, branch-free, arch-independent.
//! - Cranelift owns register allocation and instruction selection; this pass owns
//!   representation choice, the NaN-box ABI, and the deopt/Return value contract.
//!
//! # See also
//! - [`super::abi`] — `Repr` → CLIF type and the NaN-box / `JitCtx` constants.
//! - [`super::deopt`] — boxing and the side-exit bail.

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::{Block, BlockArg, InstBuilder, MemFlagsData, Value, types};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::JITModule;
use cranelift_module::{FuncId, Module};
use rustc_hash::FxHashMap;

use super::Unsupported;
use super::abi::{clif_type, REGS_OFFSET, STATUS_RETURNED, TAG_INT32, UNDEFINED_BITS};
use super::deopt::{Flags, box_tagged, emit_bail};
use crate::optimizing::deopt::DeoptPoint;
use crate::optimizing::ir::{
    BlockId, CmpOp, Graph, NodeId, NodeKind, Repr, Terminator,
};

/// Declare, lower, and define `graph` as a single Cranelift function in `module`,
/// returning its `FuncId` and the finalized code size in bytes.
pub(super) fn compile_function(
    module: &mut JITModule,
    graph: &Graph,
    frames: &FxHashMap<NodeId, DeoptPoint>,
    block_deopts: &FxHashMap<BlockId, DeoptPoint>,
) -> Result<(FuncId, usize), Unsupported> {
    let mut sig = module.make_signature();
    sig.params.push(cranelift_codegen::ir::AbiParam::new(types::I64));
    sig.returns
        .push(cranelift_codegen::ir::AbiParam::new(types::I64));
    sig.returns
        .push(cranelift_codegen::ir::AbiParam::new(types::I64));
    let func_id = module
        .declare_anonymous_function(&sig)
        .map_err(|_| Unsupported::Unlowered("clif: declare function"))?;

    let mut cctx = module.make_context();
    cctx.func.signature = sig;
    let mut fbctx = FunctionBuilderContext::new();
    {
        let mut builder = FunctionBuilder::new(&mut cctx.func, &mut fbctx);
        let mut lower = Lower::new(&mut builder, graph, frames, block_deopts);
        lower.run()?;
        builder.finalize();
    }

    module
        .define_function(func_id, &mut cctx)
        .map_err(|_| Unsupported::Unlowered("clif: define function"))?;
    let size = cctx
        .compiled_code()
        .map_or(0, |c| c.code_info().total_size as usize);
    module.clear_context(&mut cctx);
    Ok((func_id, size))
}

/// Per-function lowering state.
struct Lower<'a, 'f> {
    b: &'a mut FunctionBuilder<'f>,
    graph: &'a Graph,
    frames: &'a FxHashMap<NodeId, DeoptPoint>,
    block_deopts: &'a FxHashMap<BlockId, DeoptPoint>,
    /// Cranelift block per ir `BlockId`; `None` for blocks the builder left
    /// terminator-less (unreachable in an OSR build).
    clif_blocks: Vec<Option<Block>>,
    /// SSA node id → its Cranelift value.
    values: Vec<Option<Value>>,
    /// One cold side-exit block per deopt-capable node, created on demand.
    deopt_blocks: FxHashMap<NodeId, Block>,
    /// Deopt-capable nodes whose cold exit body is still to be filled.
    pending_deopts: Vec<NodeId>,
    /// Register-window base pointer (`JitCtx.regs`), live for the whole function.
    regs_base: Value,
    /// The `*mut JitCtx` entry parameter.
    ctx_ptr: Value,
    /// Interned memory-access flags reused across this function.
    flags: Flags,
}

impl<'a, 'f> Lower<'a, 'f> {
    fn new(
        b: &'a mut FunctionBuilder<'f>,
        graph: &'a Graph,
        frames: &'a FxHashMap<NodeId, DeoptPoint>,
        block_deopts: &'a FxHashMap<BlockId, DeoptPoint>,
    ) -> Self {
        let flags = Flags {
            trusted: MemFlagsData::trusted(),
            plain: MemFlagsData::new(),
        };
        Self {
            b,
            graph,
            frames,
            block_deopts,
            clif_blocks: vec![None; graph.blocks.len()],
            values: vec![None; graph.nodes.len()],
            deopt_blocks: FxHashMap::default(),
            pending_deopts: Vec::new(),
            // Placeholders; set once the entry block exists.
            regs_base: Value::from_u32(0),
            ctx_ptr: Value::from_u32(0),
            flags,
        }
    }

    fn run(&mut self) -> Result<(), Unsupported> {
        // Create a Cranelift block for every block the builder kept (those with a
        // terminator), plus the entry; declare phi block parameters up front so
        // edge arguments can reference them.
        for (bid, block) in self.graph.blocks.iter().enumerate() {
            if block.term.is_none() && bid as BlockId != self.graph.entry {
                continue;
            }
            let cb = self.b.create_block();
            self.clif_blocks[bid] = Some(cb);
            // The graph entry carries the `*mut JitCtx` function parameter; its
            // phis (none in practice — it has no predecessors) are never params.
            if bid as BlockId == self.graph.entry {
                continue;
            }
            for &phi in &block.phis {
                let ty = clif_type(self.graph.node(phi).repr);
                let p = self.b.append_block_param(cb, ty);
                self.values[phi as usize] = Some(p);
            }
        }

        // Entry: bind the function parameter and load the register-window base.
        let entry_cb = self.clif_blocks[self.graph.entry as usize]
            .ok_or(Unsupported::Unlowered("clif: missing entry block"))?;
        self.b.append_block_params_for_function_params(entry_cb);
        self.b.switch_to_block(entry_cb);
        self.ctx_ptr = self.b.block_params(entry_cb)[0];
        self.regs_base =
            self.b
                .ins()
                .load(types::I64, self.flags.trusted, self.ctx_ptr, REGS_OFFSET);
        self.lower_block(self.graph.entry)?;

        // Remaining reachable blocks.
        for bid in 0..self.graph.blocks.len() as BlockId {
            if bid == self.graph.entry {
                continue;
            }
            if self.clif_blocks[bid as usize].is_none() {
                continue;
            }
            let cb = self.clif_blocks[bid as usize].unwrap();
            self.b.switch_to_block(cb);
            self.lower_block(bid)?;
        }

        self.fill_deopt_exits()?;
        self.b.seal_all_blocks();
        Ok(())
    }

    /// Lower one block's body nodes and its terminator. The current Cranelift
    /// block is already selected by the caller.
    fn lower_block(&mut self, bid: BlockId) -> Result<(), Unsupported> {
        let block = self.graph.block(bid);
        for &nid in &block.body {
            self.lower_node(nid)?;
        }
        match block.term {
            Some(Terminator::Return(v)) => {
                let boxed = self.boxed(v)?;
                let status = self.b.ins().iconst(types::I64, STATUS_RETURNED);
                self.b.ins().return_(&[boxed, status]);
            }
            Some(Terminator::Jump(target)) => {
                let args = self.edge_args(bid, target)?;
                let tb = self.clif_block(target)?;
                self.b.ins().jump(tb, &args);
            }
            Some(Terminator::Branch {
                cond,
                on_true,
                on_false,
            }) => {
                let cv = self.val(cond)?;
                let true_args = self.edge_args(bid, on_true)?;
                let false_args = self.edge_args(bid, on_false)?;
                let tb = self.clif_block(on_true)?;
                let fb = self.clif_block(on_false)?;
                self.b.ins().brif(cv, tb, &true_args, fb, &false_args);
            }
            Some(Terminator::Deopt(_)) => {
                let point = self
                    .block_deopts
                    .get(&bid)
                    .ok_or(Unsupported::Unlowered("clif: deopt terminator w/o frame"))?;
                emit_bail(
                    self.b,
                    self.flags,
                    self.ctx_ptr,
                    self.regs_base,
                    point,
                    self.graph,
                    &self.values,
                )?;
            }
            None => return Err(Unsupported::Unlowered("clif: block missing terminator")),
        }
        Ok(())
    }

    /// Phi inputs for the `pred → succ` edge, boxed to tagged `Value`s to match
    /// the successor's tagged block parameters.
    fn edge_args(&mut self, pred: BlockId, succ: BlockId) -> Result<Vec<BlockArg>, Unsupported> {
        let sblock = self.graph.block(succ);
        if sblock.phis.is_empty() {
            return Ok(Vec::new());
        }
        let idx = sblock
            .preds
            .iter()
            .position(|&p| p == pred)
            .ok_or(Unsupported::Unlowered("clif: edge not in succ preds"))?;
        let mut args = Vec::with_capacity(sblock.phis.len());
        for &phi in &sblock.phis {
            let input = match &self.graph.node(phi).kind {
                NodeKind::Phi(ops) => *ops
                    .get(idx)
                    .ok_or(Unsupported::Unlowered("clif: phi operand index"))?,
                _ => return Err(Unsupported::Unlowered("clif: non-phi in phis")),
            };
            args.push(BlockArg::from(self.boxed(input)?));
        }
        Ok(args)
    }

    /// Look up a lowered SSA value.
    fn val(&self, nid: NodeId) -> Result<Value, Unsupported> {
        self.values[nid as usize].ok_or(Unsupported::Unlowered("clif: value used before def"))
    }

    /// The tagged (`i64`) boxing of an SSA value.
    fn boxed(&mut self, nid: NodeId) -> Result<Value, Unsupported> {
        let v = self.val(nid)?;
        Ok(box_tagged(self.b, self.flags, v, self.graph.node(nid).repr))
    }

    fn clif_block(&self, bid: BlockId) -> Result<Block, Unsupported> {
        self.clif_blocks[bid as usize].ok_or(Unsupported::Unlowered("clif: branch to dead block"))
    }

    /// The cold side-exit block for deopt-capable node `nid`, created on demand.
    fn deopt_block(&mut self, nid: NodeId) -> Result<Block, Unsupported> {
        if !self.frames.contains_key(&nid) {
            return Err(Unsupported::Unlowered("clif: guard without deopt frame"));
        }
        if let Some(&blk) = self.deopt_blocks.get(&nid) {
            return Ok(blk);
        }
        let blk = self.b.create_block();
        self.deopt_blocks.insert(nid, blk);
        self.pending_deopts.push(nid);
        Ok(blk)
    }

    /// Fill every cold side-exit block: store the live registers, stamp `bail_pc`,
    /// and return `Bailed`. Marked cold so Cranelift lays them off the hot path.
    fn fill_deopt_exits(&mut self) -> Result<(), Unsupported> {
        let pending = std::mem::take(&mut self.pending_deopts);
        for nid in pending {
            let blk = self.deopt_blocks[&nid];
            let point = self
                .frames
                .get(&nid)
                .ok_or(Unsupported::Unlowered("clif: exit without frame"))?;
            self.b.switch_to_block(blk);
            self.b.set_cold_block(blk);
            emit_bail(
                self.b,
                self.flags,
                self.ctx_ptr,
                self.regs_base,
                point,
                self.graph,
                &self.values,
            )?;
        }
        Ok(())
    }

    /// Branch to the guard's cold exit when `fail` (an `i8` predicate) is set,
    /// continuing in a fresh block. Returns the continuation block (already
    /// selected) so the guard's success path proceeds there.
    fn guard(&mut self, nid: NodeId, fail: Value) -> Result<(), Unsupported> {
        let exit = self.deopt_block(nid)?;
        let cont = self.b.create_block();
        self.b.ins().brif(fail, exit, &[], cont, &[]);
        self.b.switch_to_block(cont);
        Ok(())
    }

    fn lower_node(&mut self, nid: NodeId) -> Result<(), Unsupported> {
        let node = self.graph.node(nid);
        let result: Option<Value> = match &node.kind {
            // Phis are block parameters; entry params have no body code.
            NodeKind::Phi(_) => None,
            NodeKind::Param(reg) => {
                let off = i32::from(*reg) * 8;
                Some(
                    self.b
                        .ins()
                        .load(types::I64, self.flags.trusted, self.regs_base, off),
                )
            }
            NodeKind::ConstInt32(v) => Some(self.b.ins().iconst(types::I32, i64::from(*v))),
            NodeKind::ConstF64(v) => Some(self.b.ins().f64const(*v)),
            NodeKind::ConstBool(bln) => {
                let bits = if *bln {
                    super::abi::TRUE_BITS
                } else {
                    super::abi::FALSE_BITS
                };
                Some(self.b.ins().iconst(types::I64, bits as i64))
            }
            NodeKind::ConstUndefined => Some(self.b.ins().iconst(types::I64, UNDEFINED_BITS as i64)),
            NodeKind::CheckInt32(operand) => Some(self.lower_check_int32(nid, *operand)?),
            NodeKind::CheckNumber(operand) => Some(self.lower_check_number(nid, *operand)?),
            NodeKind::Int32ToFloat64(operand) => {
                let v = self.val(*operand)?;
                Some(self.b.ins().fcvt_from_sint(types::F64, v))
            }
            NodeKind::Int32Add(a, b) | NodeKind::Int32Sub(a, b) | NodeKind::Int32Mul(a, b) => {
                Some(self.lower_int32_overflow(nid, &node.kind, *a, *b)?)
            }
            NodeKind::Int32BitOr(a, b) => {
                let (x, y) = (self.val(*a)?, self.val(*b)?);
                Some(self.b.ins().bor(x, y))
            }
            NodeKind::Int32BitAnd(a, b) => {
                let (x, y) = (self.val(*a)?, self.val(*b)?);
                Some(self.b.ins().band(x, y))
            }
            NodeKind::Int32BitXor(a, b) => {
                let (x, y) = (self.val(*a)?, self.val(*b)?);
                Some(self.b.ins().bxor(x, y))
            }
            NodeKind::Int32Shl(a, b) => {
                let (x, y) = (self.val(*a)?, self.val(*b)?);
                Some(self.b.ins().ishl(x, y))
            }
            NodeKind::Int32Shr(a, b) => {
                let (x, y) = (self.val(*a)?, self.val(*b)?);
                Some(self.b.ins().sshr(x, y))
            }
            NodeKind::Int32UshrToFloat64(a, b) => {
                let (x, y) = (self.val(*a)?, self.val(*b)?);
                let shifted = self.b.ins().ushr(x, y);
                Some(self.b.ins().fcvt_from_uint(types::F64, shifted))
            }
            NodeKind::Int32Compare(op, a, b) => {
                let (x, y) = (self.val(*a)?, self.val(*b)?);
                Some(self.b.ins().icmp(int_cc(*op), x, y))
            }
            NodeKind::Float64Add(a, b) => {
                let (x, y) = (self.val(*a)?, self.val(*b)?);
                Some(self.b.ins().fadd(x, y))
            }
            NodeKind::Float64Sub(a, b) => {
                let (x, y) = (self.val(*a)?, self.val(*b)?);
                Some(self.b.ins().fsub(x, y))
            }
            NodeKind::Float64Mul(a, b) => {
                let (x, y) = (self.val(*a)?, self.val(*b)?);
                Some(self.b.ins().fmul(x, y))
            }
            NodeKind::Float64Div(a, b) => {
                let (x, y) = (self.val(*a)?, self.val(*b)?);
                Some(self.b.ins().fdiv(x, y))
            }
            NodeKind::Float64Compare(op, a, b) => {
                let (x, y) = (self.val(*a)?, self.val(*b)?);
                Some(self.b.ins().fcmp(float_cc(*op), x, y))
            }
            _ => return Err(Unsupported::Unlowered("clif: node kind outside S0 subset")),
        };
        if let Some(v) = result {
            self.values[nid as usize] = Some(v);
        }
        Ok(())
    }

    /// `CheckInt32`: pass an already-`Int32` operand through; guard a `Tagged`
    /// operand's tag is [`TAG_INT32`] (deopt otherwise) and unbox to `i32`.
    fn lower_check_int32(&mut self, nid: NodeId, operand: NodeId) -> Result<Value, Unsupported> {
        let orepr = self.graph.node(operand).repr;
        let v = self.val(operand)?;
        match orepr {
            Repr::Int32 => Ok(v),
            Repr::Tagged => {
                let top16 = self.b.ins().ushr_imm(v, 48);
                let is_int = self.b.ins().icmp_imm(IntCC::Equal, top16, TAG_INT32 as i64);
                // Deopt when not int32-tagged.
                let not_int = self.b.ins().icmp_imm(IntCC::Equal, is_int, 0);
                self.guard(nid, not_int)?;
                Ok(self.b.ins().ireduce(types::I32, v))
            }
            _ => Err(Unsupported::Unlowered("clif: check-int32 operand not tagged")),
        }
    }

    /// `CheckNumber`: widen an `Int32` operand, copy a `Float64`, or guard+unbox a
    /// `Tagged` operand to `f64` (an int32 tag widens; a special/pointer tag
    /// deopts; every other prefix is a double, taken verbatim).
    fn lower_check_number(&mut self, nid: NodeId, operand: NodeId) -> Result<Value, Unsupported> {
        let orepr = self.graph.node(operand).repr;
        let v = self.val(operand)?;
        match orepr {
            Repr::Float64 => Ok(v),
            Repr::Int32 => Ok(self.b.ins().fcvt_from_sint(types::F64, v)),
            Repr::Tagged => {
                let top16 = self.b.ins().ushr_imm(v, 48);
                let is_int = self.b.ins().icmp_imm(IntCC::Equal, top16, TAG_INT32 as i64);
                // A special / pointer tag (0x7FFA..=0x7FFF) is a non-number: deopt.
                let ge = self
                    .b
                    .ins()
                    .icmp_imm(IntCC::UnsignedGreaterThanOrEqual, top16, 0x7FFA);
                let le = self
                    .b
                    .ins()
                    .icmp_imm(IntCC::UnsignedLessThanOrEqual, top16, 0x7FFF);
                let bad = self.b.ins().band(ge, le);
                self.guard(nid, bad)?;
                // int32: widen the signed low-32 payload; double: bits verbatim.
                let as_i32 = self.b.ins().ireduce(types::I32, v);
                let from_int = self.b.ins().fcvt_from_sint(types::F64, as_i32);
                let from_bits = self.b.ins().bitcast(types::F64, self.flags.plain, v);
                Ok(self.b.ins().select(is_int, from_int, from_bits))
            }
            Repr::Bool => Err(Unsupported::Unlowered("clif: check-number operand bool")),
        }
    }

    /// `Int32Add`/`Sub`/`Mul` with signed-overflow deopt: compute in `i64`,
    /// reduce to `i32`, and deopt when the reduced result does not re-sign-extend
    /// to the wide result.
    fn lower_int32_overflow(
        &mut self,
        nid: NodeId,
        kind: &NodeKind,
        a: NodeId,
        b: NodeId,
    ) -> Result<Value, Unsupported> {
        let av = self.val(a)?;
        let bv = self.val(b)?;
        let a64 = self.b.ins().sextend(types::I64, av);
        let b64 = self.b.ins().sextend(types::I64, bv);
        let wide = match kind {
            NodeKind::Int32Add(_, _) => self.b.ins().iadd(a64, b64),
            NodeKind::Int32Sub(_, _) => self.b.ins().isub(a64, b64),
            NodeKind::Int32Mul(_, _) => self.b.ins().imul(a64, b64),
            _ => unreachable!("int32 overflow lowering on non-arith node"),
        };
        let narrow = self.b.ins().ireduce(types::I32, wide);
        let re_wide = self.b.ins().sextend(types::I64, narrow);
        let overflow = self.b.ins().icmp(IntCC::NotEqual, re_wide, wide);
        self.guard(nid, overflow)?;
        Ok(narrow)
    }
}

/// Map a typed-SSA comparison to a signed integer condition code.
fn int_cc(op: CmpOp) -> IntCC {
    match op {
        CmpOp::Lt => IntCC::SignedLessThan,
        CmpOp::Le => IntCC::SignedLessThanOrEqual,
        CmpOp::Gt => IntCC::SignedGreaterThan,
        CmpOp::Ge => IntCC::SignedGreaterThanOrEqual,
        CmpOp::Eq => IntCC::Equal,
        CmpOp::Ne => IntCC::NotEqual,
    }
}

/// Map a typed-SSA comparison to an IEEE float condition code. The relational
/// codes are ordered (a `NaN` operand yields `false`); `Ne` is unordered-or-not
/// equal (a `NaN` operand yields `true`), matching JS number comparison and the
/// dynasm tier.
fn float_cc(op: CmpOp) -> FloatCC {
    match op {
        CmpOp::Lt => FloatCC::LessThan,
        CmpOp::Le => FloatCC::LessThanOrEqual,
        CmpOp::Gt => FloatCC::GreaterThan,
        CmpOp::Ge => FloatCC::GreaterThanOrEqual,
        CmpOp::Eq => FloatCC::Equal,
        CmpOp::Ne => FloatCC::NotEqual,
    }
}
