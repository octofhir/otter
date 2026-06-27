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

use otter_vm::JitFunctionView;

use super::Unsupported;
use super::abi::{
    HOLE_BITS, NULL_BITS, REGS_OFFSET, STATUS_RETURNED, TAG_INT32, TAG_PTR_OBJECT, UNDEFINED_BITS,
    clif_type,
};
use super::deopt::{Flags, box_tagged, emit_bail};
use crate::optimizing::deopt::DeoptPoint;
use crate::optimizing::ir::{BlockId, CmpOp, Graph, NodeId, NodeKind, Repr, Terminator};

/// Declare, lower, and define `graph` as a single Cranelift function in `module`,
/// returning its `FuncId` and the finalized code size in bytes.
pub(super) fn compile_function(
    module: &mut JITModule,
    view: &JitFunctionView,
    graph: &Graph,
    frames: &FxHashMap<NodeId, DeoptPoint>,
    block_deopts: &FxHashMap<BlockId, DeoptPoint>,
) -> Result<(FuncId, usize), Unsupported> {
    let mut sig = module.make_signature();
    sig.params
        .push(cranelift_codegen::ir::AbiParam::new(types::I64));
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
        let mut lower = Lower::new(&mut builder, view, graph, frames, block_deopts);
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
    view: &'a JitFunctionView,
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
        view: &'a JitFunctionView,
        graph: &'a Graph,
        frames: &'a FxHashMap<NodeId, DeoptPoint>,
        block_deopts: &'a FxHashMap<BlockId, DeoptPoint>,
    ) -> Self {
        let flags = Flags {
            trusted: MemFlagsData::trusted(),
            readonly: MemFlagsData::trusted().with_readonly(),
            plain: MemFlagsData::new(),
        };
        Self {
            b,
            view,
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
            NodeKind::ConstUndefined => {
                Some(self.b.ins().iconst(types::I64, UNDEFINED_BITS as i64))
            }
            NodeKind::ConstNull => Some(self.b.ins().iconst(types::I64, NULL_BITS as i64)),
            NodeKind::CheckInt32(operand) => Some(self.lower_check_int32(nid, *operand)?),
            NodeKind::CheckNumber(operand) => Some(self.lower_check_number(nid, *operand)?),
            NodeKind::Int32ToFloat64(operand) => {
                let v = self.val(*operand)?;
                Some(self.b.ins().fcvt_from_sint(types::F64, v))
            }
            NodeKind::Float64ToInt32(operand) => Some(self.lower_float64_to_int32(*operand)?),
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
            NodeKind::TaggedIsNull { value, negate } => {
                let v = self.val(*value)?;
                let null = self.b.ins().iconst(types::I64, NULL_BITS as i64);
                let cc = if *negate {
                    IntCC::NotEqual
                } else {
                    IntCC::Equal
                };
                Some(self.b.ins().icmp(cc, v, null))
            }
            NodeKind::LoadArrayLength(recv) => Some(self.lower_array_length(nid, *recv)?),
            NodeKind::LoadElement(recv, idx) => Some(self.lower_load_element(nid, *recv, *idx)?),
            NodeKind::StoreElement(recv, idx, val) => {
                self.lower_store_element(nid, *recv, *idx, *val)?;
                None
            }
            _ => return Err(Unsupported::Unlowered("clif: node kind outside subset")),
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
            _ => Err(Unsupported::Unlowered(
                "clif: check-int32 operand not tagged",
            )),
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

    /// ECMAScript `ToInt32` for an unboxed `f64`, implemented from IEEE-754 bits:
    /// truncate toward zero, take modulo 2^32, then interpret the low bits as
    /// signed int32. NaN, infinities, ±0, and values whose integer part is a
    /// multiple of 2^32 produce +0.
    fn lower_float64_to_int32(&mut self, operand: NodeId) -> Result<Value, Unsupported> {
        if self.graph.node(operand).repr != Repr::Float64 {
            return Err(Unsupported::Unlowered("clif: to-int32 operand not float64"));
        }
        let v = self.val(operand)?;
        let bits = self.b.ins().bitcast(types::I64, self.flags.plain, v);
        let sign = self.b.ins().icmp_imm(IntCC::SignedLessThan, bits, 0);
        let exp = {
            let shifted = self.b.ins().ushr_imm(bits, 52);
            self.b.ins().band_imm(shifted, 0x7ff)
        };
        let exp_small = self
            .b
            .ins()
            .icmp_imm(IntCC::UnsignedLessThanOrEqual, exp, 1022);
        let exp_nan_inf = self.b.ins().icmp_imm(IntCC::Equal, exp, 0x7ff);
        let exp_huge_zero = self
            .b
            .ins()
            .icmp_imm(IntCC::UnsignedGreaterThanOrEqual, exp, 1107);
        let zeroish_a = self.b.ins().bor(exp_small, exp_nan_inf);
        let zeroish = self.b.ins().bor(zeroish_a, exp_huge_zero);
        let zero = self.b.ins().iconst(types::I32, 0);
        let mantissa = {
            let mant = self.b.ins().band_imm(bits, 0x000f_ffff_ffff_ffff);
            self.b.ins().bor_imm(mant, 1_i64 << 52)
        };

        let zero_block = self.b.create_block();
        let left_block = self.b.create_block();
        let right_block = self.b.create_block();
        let done = self.b.create_block();
        let done_arg = self.b.append_block_param(done, types::I32);

        let left_shift = self
            .b
            .ins()
            .icmp_imm(IntCC::UnsignedGreaterThanOrEqual, exp, 1075);
        let dispatch = self.b.create_block();
        self.b.ins().brif(zeroish, zero_block, &[], dispatch, &[]);

        self.b.switch_to_block(dispatch);
        self.b
            .ins()
            .brif(left_shift, left_block, &[], right_block, &[]);

        self.b.switch_to_block(zero_block);
        self.b.ins().jump(done, &[BlockArg::from(zero)]);

        self.b.switch_to_block(left_block);
        let shift = self.b.ins().iadd_imm(exp, -1075);
        let shifted = self.b.ins().ishl(mantissa, shift);
        let low = self.b.ins().ireduce(types::I32, shifted);
        self.b.ins().jump(done, &[BlockArg::from(low)]);

        self.b.switch_to_block(right_block);
        let right_base = self.b.ins().iconst(types::I64, 1075);
        let shift = self.b.ins().isub(right_base, exp);
        let shifted = self.b.ins().ushr(mantissa, shift);
        let low = self.b.ins().ireduce(types::I32, shifted);
        self.b.ins().jump(done, &[BlockArg::from(low)]);

        self.b.switch_to_block(done);
        let neg = self.b.ins().isub(zero, done_arg);
        Ok(self.b.ins().select(sign, neg, done_arg))
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

    /// A trusted load of `ty` at `ptr + off` (mutable memory).
    fn ld(&mut self, ty: cranelift_codegen::ir::Type, ptr: Value, off: u32) -> Value {
        self.b.ins().load(ty, self.flags.trusted, ptr, off as i32)
    }

    /// A readonly load of `ty` at `ptr + off`: a never-written field (object /
    /// buffer metadata). The `readonly` flag lets Cranelift GVN/LICM dedup and
    /// hoist it across element stores and out of loops.
    fn ldro(&mut self, ty: cranelift_codegen::ir::Type, ptr: Value, off: u32) -> Value {
        self.b.ins().load(ty, self.flags.readonly, ptr, off as i32)
    }

    /// Decompress a tagged object receiver into its GC pointer and GC-header type
    /// tag, deopting at `nid` when the receiver is not an object pointer. Returns
    /// `(gc_ptr: i64, type_tag: i8)`. The builder is left on the success path.
    fn recv_body(&mut self, nid: NodeId, recv: NodeId) -> Result<(Value, Value), Unsupported> {
        let v = self.val(recv)?;
        let top16 = self.b.ins().ushr_imm(v, 48);
        let is_obj = self
            .b
            .ins()
            .icmp_imm(IntCC::Equal, top16, TAG_PTR_OBJECT as i64);
        let not_obj = self.b.ins().icmp_imm(IntCC::Equal, is_obj, 0);
        self.guard(nid, not_obj)?;
        // Decompress: GcHeader ptr = cage_base + low32(value).
        let low = self.b.ins().ireduce(types::I32, v);
        let off = self.b.ins().uextend(types::I64, low);
        let cage = self.b.ins().iconst(types::I64, self.view.cage_base as i64);
        let ptr = self.b.ins().iadd(cage, off);
        let tag = self.ldro(types::I8, ptr, 0);
        Ok((ptr, tag))
    }

    /// `LoadArrayLength`: deopt unless the receiver is a dense Array whose length
    /// fits int32, then return that length unboxed.
    fn lower_array_length(&mut self, nid: NodeId, recv: NodeId) -> Result<Value, Unsupported> {
        let array_tag = i64::from(self.view.ta_layout.array_type_tag);
        let length_byte = self.view.ta_layout.array_length_byte;
        let (ptr, tag) = self.recv_body(nid, recv)?;
        let not_arr = self.b.ins().icmp_imm(IntCC::NotEqual, tag, array_tag);
        self.guard(nid, not_arr)?;
        let len = self.ld(types::I64, ptr, length_byte);
        let too_big = self
            .b
            .ins()
            .icmp_imm(IntCC::UnsignedGreaterThan, len, i64::from(i32::MAX));
        self.guard(nid, too_big)?;
        Ok(self.b.ins().ireduce(types::I32, len))
    }

    /// `LoadElement`: speculative dense-array / typed-array `recv[idx]`. Any miss
    /// (non-object, wrong body, OOB, hole, unsupported kind) deopts at `nid`.
    /// Result is the tagged element value, produced as the `done` block parameter.
    fn lower_load_element(
        &mut self,
        nid: NodeId,
        recv: NodeId,
        idx: NodeId,
    ) -> Result<Value, Unsupported> {
        let l = self.view.ta_layout;
        let (ptr_word, len_word) = vec_layout_offsets();
        let (ptr, tag) = self.recv_body(nid, recv)?;
        let idxv = self.val(idx)?;
        let index = self.b.ins().uextend(types::I64, idxv);

        let done = self.b.create_block();
        let result = self.b.append_block_param(done, types::I64);
        let array_blk = self.b.create_block();
        let chk_ta = self.b.create_block();
        let ta_blk = self.b.create_block();
        let f64_blk = self.b.create_block();
        let chk_i32 = self.b.create_block();
        let i32_blk = self.b.create_block();
        let deopt = self.deopt_block(nid)?;

        let is_array = self
            .b
            .ins()
            .icmp_imm(IntCC::Equal, tag, i64::from(l.array_type_tag));
        self.b.ins().brif(is_array, array_blk, &[], chk_ta, &[]);

        // Ordinary dense array: bounds-check the elements `Vec`, load the raw
        // `Value`, deopt on a hole.
        self.b.switch_to_block(array_blk);
        let alen = self.ld(types::I64, ptr, l.array_elements_byte + len_word);
        let oob = self
            .b
            .ins()
            .icmp(IntCC::UnsignedGreaterThanOrEqual, index, alen);
        self.guard(nid, oob)?;
        let data = self.ld(types::I64, ptr, l.array_elements_byte + ptr_word);
        let elptr = self.scaled_addr(data, index, 3);
        let elem = self.ld(types::I64, elptr, 0);
        let hole = self.b.ins().iconst(types::I64, HOLE_BITS as i64);
        let is_hole = self.b.ins().icmp(IntCC::Equal, elem, hole);
        self.guard(nid, is_hole)?;
        self.b.ins().jump(done, &[BlockArg::from(elem)]);

        // Not a dense array: must be a typed array, else deopt.
        self.b.switch_to_block(chk_ta);
        let is_ta = self
            .b
            .ins()
            .icmp_imm(IntCC::Equal, tag, i64::from(l.ta_type_tag));
        self.b.ins().brif(is_ta, ta_blk, &[], deopt, &[]);

        // Typed array: walk to the backing `Vec<u8>`, then split on element kind.
        self.b.switch_to_block(ta_blk);
        let (bytes_ptr, bytes_len, byte_off, kind) =
            self.ta_buffer(nid, ptr, index, ptr_word, len_word)?;
        let is_f64 = self
            .b
            .ins()
            .icmp_imm(IntCC::Equal, kind, i64::from(l.kind_float64));
        self.b.ins().brif(is_f64, f64_blk, &[], chk_i32, &[]);

        self.b.switch_to_block(chk_i32);
        let is_i32 = self
            .b
            .ins()
            .icmp_imm(IntCC::Equal, kind, i64::from(l.kind_int32));
        self.b.ins().brif(is_i32, i32_blk, &[], deopt, &[]);

        // Float64 element: byte-range check, load the `f64`, NaN-box it.
        self.b.switch_to_block(f64_blk);
        let eoff = self.elem_byte_off(index, byte_off, 3);
        let end = self.b.ins().iadd_imm(eoff, 8);
        let over = self
            .b
            .ins()
            .icmp(IntCC::UnsignedGreaterThan, end, bytes_len);
        self.guard(nid, over)?;
        let addr = self.b.ins().iadd(bytes_ptr, eoff);
        let d = self.ld(types::F64, addr, 0);
        let boxed = box_tagged(self.b, self.flags, d, Repr::Float64);
        self.b.ins().jump(done, &[BlockArg::from(boxed)]);

        // Int32 element: byte-range check, load the `i32`, box it.
        self.b.switch_to_block(i32_blk);
        let eoff = self.elem_byte_off(index, byte_off, 2);
        let end = self.b.ins().iadd_imm(eoff, 4);
        let over = self
            .b
            .ins()
            .icmp(IntCC::UnsignedGreaterThan, end, bytes_len);
        self.guard(nid, over)?;
        let addr = self.b.ins().iadd(bytes_ptr, eoff);
        let w = self.ld(types::I32, addr, 0);
        let boxed = box_tagged(self.b, self.flags, w, Repr::Int32);
        self.b.ins().jump(done, &[BlockArg::from(boxed)]);

        self.b.switch_to_block(done);
        Ok(result)
    }

    /// `StoreElement`: speculative dense-array / typed-array `recv[idx] = value`.
    /// Every miss deopts at `nid`. Side-effect only.
    fn lower_store_element(
        &mut self,
        nid: NodeId,
        recv: NodeId,
        idx: NodeId,
        val: NodeId,
    ) -> Result<(), Unsupported> {
        let l = self.view.ta_layout;
        let (ptr_word, len_word) = vec_layout_offsets();
        let vrepr = self.graph.node(val).repr;
        let (ptr, tag) = self.recv_body(nid, recv)?;
        let idxv = self.val(idx)?;
        let index = self.b.ins().uextend(types::I64, idxv);

        let done = self.b.create_block();
        let array_blk = self.b.create_block();
        let ta_blk = self.b.create_block();
        let f64_blk = self.b.create_block();
        let chk_i32 = self.b.create_block();
        let i32_blk = self.b.create_block();
        let deopt = self.deopt_block(nid)?;

        let is_array = self
            .b
            .ins()
            .icmp_imm(IntCC::Equal, tag, i64::from(l.array_type_tag));
        self.b.ins().brif(is_array, array_blk, &[], ta_blk, &[]);

        // Dense array: a live array-index protector, an exotic sidecar, or an
        // out-of-bounds index all make the store observable / spec-bound — deopt.
        self.b.switch_to_block(array_blk);
        let prot_ptr = self.ld(
            types::I64,
            self.ctx_ptr,
            super::abi::ARRAY_INDEX_ACCESSOR_PROTECTOR_PTR_OFFSET,
        );
        let prot = self.ld(types::I8, prot_ptr, 0);
        let prot_live = self.b.ins().icmp_imm(IntCC::NotEqual, prot, 0);
        self.guard(nid, prot_live)?;
        let exotic = self.ld(types::I64, ptr, l.array_exotic_byte);
        let has_exotic = self.b.ins().icmp_imm(IntCC::NotEqual, exotic, 0);
        self.guard(nid, has_exotic)?;
        let elen = self.ld(types::I64, ptr, l.array_elements_byte + len_word);
        let oob = self
            .b
            .ins()
            .icmp(IntCC::UnsignedGreaterThanOrEqual, index, elen);
        self.guard(nid, oob)?;
        let llen = self.ld(types::I64, ptr, l.array_length_byte);
        let oob2 = self
            .b
            .ins()
            .icmp(IntCC::UnsignedGreaterThanOrEqual, index, llen);
        self.guard(nid, oob2)?;
        let data = self.ld(types::I64, ptr, l.array_elements_byte + ptr_word);
        let slot = self.scaled_addr(data, index, 3);
        let boxed = self.boxed_val(val, vrepr)?;
        self.b.ins().store(self.flags.trusted, boxed, slot, 0);
        self.b.ins().jump(done, &[]);

        // Typed array: walk to the backing `Vec<u8>`, split on kind, store raw.
        self.b.switch_to_block(ta_blk);
        let is_ta = self
            .b
            .ins()
            .icmp_imm(IntCC::Equal, tag, i64::from(l.ta_type_tag));
        let ta_walk = self.b.create_block();
        self.b.ins().brif(is_ta, ta_walk, &[], deopt, &[]);
        self.b.switch_to_block(ta_walk);
        let (bytes_ptr, bytes_len, byte_off, kind) =
            self.ta_buffer(nid, ptr, index, ptr_word, len_word)?;
        let is_f64 = self
            .b
            .ins()
            .icmp_imm(IntCC::Equal, kind, i64::from(l.kind_float64));
        self.b.ins().brif(is_f64, f64_blk, &[], chk_i32, &[]);

        self.b.switch_to_block(chk_i32);
        let is_i32 = self
            .b
            .ins()
            .icmp_imm(IntCC::Equal, kind, i64::from(l.kind_int32));
        self.b.ins().brif(is_i32, i32_blk, &[], deopt, &[]);

        // Float64 store: range-check, coerce the value to `f64`, store.
        self.b.switch_to_block(f64_blk);
        let eoff = self.elem_byte_off(index, byte_off, 3);
        let end = self.b.ins().iadd_imm(eoff, 8);
        let over = self
            .b
            .ins()
            .icmp(IntCC::UnsignedGreaterThan, end, bytes_len);
        self.guard(nid, over)?;
        let addr = self.b.ins().iadd(bytes_ptr, eoff);
        let d = self.value_as_f64(val, vrepr)?;
        self.b.ins().store(self.flags.trusted, d, addr, 0);
        self.b.ins().jump(done, &[]);

        // Int32 store: range-check, store the low 32 bits (an `f64` value misses).
        self.b.switch_to_block(i32_blk);
        let w = match vrepr {
            Repr::Int32 => self.val(val)?,
            _ => {
                self.b.ins().jump(deopt, &[]);
                self.b.switch_to_block(done);
                return Ok(());
            }
        };
        let eoff = self.elem_byte_off(index, byte_off, 2);
        let end = self.b.ins().iadd_imm(eoff, 4);
        let over = self
            .b
            .ins()
            .icmp(IntCC::UnsignedGreaterThan, end, bytes_len);
        self.guard(nid, over)?;
        let addr = self.b.ins().iadd(bytes_ptr, eoff);
        self.b.ins().store(self.flags.trusted, w, addr, 0);
        self.b.ins().jump(done, &[]);

        self.b.switch_to_block(done);
        Ok(())
    }

    /// `bytes_ptr + ((index << shift) + byte_off)` byte offset within a typed
    /// array's backing buffer.
    fn elem_byte_off(&mut self, index: Value, byte_off: Value, shift: i64) -> Value {
        let scaled = self.b.ins().ishl_imm(index, shift);
        self.b.ins().iadd(scaled, byte_off)
    }

    /// `base + (index << shift)` as an i64 address.
    fn scaled_addr(&mut self, base: Value, index: Value, shift: i64) -> Value {
        let scaled = self.b.ins().ishl_imm(index, shift);
        self.b.ins().iadd(base, scaled)
    }

    /// Box an SSA value (by its repr) to its tagged `Value` for a dense-array
    /// element store. The value is always a primitive, so no write barrier.
    fn boxed_val(&mut self, val: NodeId, repr: Repr) -> Result<Value, Unsupported> {
        let v = self.val(val)?;
        Ok(box_tagged(self.b, self.flags, v, repr))
    }

    /// Coerce an SSA value to `f64` for a `Float64Array` store (an int32 widens).
    fn value_as_f64(&mut self, val: NodeId, repr: Repr) -> Result<Value, Unsupported> {
        let v = self.val(val)?;
        match repr {
            Repr::Float64 => Ok(v),
            Repr::Int32 => Ok(self.b.ins().fcvt_from_sint(types::F64, v)),
            _ => Err(Unsupported::Unlowered(
                "clif: store-element value not numeric",
            )),
        }
    }

    /// Walk a typed-array body to its backing `Vec<u8>`: guard not length-tracking,
    /// bounds-check the element count, guard the buffer is a local buffer of the
    /// right body type, and return `(bytes_ptr, bytes_len, byte_offset, kind)`.
    /// Any miss deopts at `nid`.
    fn ta_buffer(
        &mut self,
        nid: NodeId,
        ptr: Value,
        index: Value,
        ptr_word: u32,
        len_word: u32,
    ) -> Result<(Value, Value, Value, Value), Unsupported> {
        let l = self.view.ta_layout;
        let tracking = self.ldro(types::I8, ptr, l.ta_length_tracking_byte);
        let is_tracking = self.b.ins().icmp_imm(IntCC::NotEqual, tracking, 0);
        self.guard(nid, is_tracking)?;
        // Spec length bound: reading/writing at or beyond the logical element
        // count must defer to the interpreter, not touch raw buffer bytes.
        let len = self.ldro(types::I64, ptr, l.ta_length_byte);
        let oob = self
            .b
            .ins()
            .icmp(IntCC::UnsignedGreaterThanOrEqual, index, len);
        self.guard(nid, oob)?;
        let disc = self.ldro(types::I32, ptr, l.buffer_disc_byte);
        let not_local = self
            .b
            .ins()
            .icmp_imm(IntCC::NotEqual, disc, i64::from(l.buffer_local_tag));
        self.guard(nid, not_local)?;
        let handle = self.ldro(types::I32, ptr, l.buffer_handle_byte);
        let hoff = self.b.ins().uextend(types::I64, handle);
        let cage = self.b.ins().iconst(types::I64, self.view.cage_base as i64);
        let bufptr = self.b.ins().iadd(cage, hoff);
        let btag = self.ldro(types::I8, bufptr, 0);
        let not_buf =
            self.b
                .ins()
                .icmp_imm(IntCC::NotEqual, btag, i64::from(l.local_buffer_type_tag));
        self.guard(nid, not_buf)?;
        let bytes_ptr = self.ldro(types::I64, bufptr, l.buf_bytes_byte + ptr_word);
        let bytes_len = self.ldro(types::I64, bufptr, l.buf_bytes_byte + len_word);
        let byte_off = self.ldro(types::I64, ptr, l.ta_byte_offset_byte);
        let kind = self.ldro(types::I32, ptr, l.ta_kind_byte);
        Ok((bytes_ptr, bytes_len, byte_off, kind))
    }
}

/// Byte offsets of a `Vec`'s data-pointer and length words, probed by value
/// identity (the standard library does not promise field order). Mirrors the
/// dynasm tier so both backends read the same `Vec<Value>` / `Vec<u8>` storage.
fn vec_layout_offsets() -> (u32, u32) {
    static CACHE: std::sync::OnceLock<(u32, u32)> = std::sync::OnceLock::new();
    *CACHE.get_or_init(|| {
        let mut v: Vec<u8> = Vec::with_capacity(4);
        v.push(0xA5);
        let ptr = v.as_ptr() as usize;
        let len = v.len();
        assert_eq!(std::mem::size_of::<Vec<u8>>(), 24);
        // SAFETY: copy the Vec's three machine words by value; they are compared
        // to known pointer/length values, never dereferenced.
        let words: [usize; 3] = unsafe { std::mem::transmute_copy(&v) };
        let mut ptr_off = None;
        let mut len_off = None;
        for (i, &w) in words.iter().enumerate() {
            if w == ptr {
                ptr_off = Some((i * 8) as u32);
            } else if w == len {
                len_off = Some((i * 8) as u32);
            }
        }
        (
            ptr_off.expect("Vec data-pointer word not found"),
            len_off.expect("Vec length word not found"),
        )
    })
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
