//! Cranelift code generator: a second backend for the optimizing tier.
//!
//! The optimizing tier builds a typed SSA [`Graph`] and captures
//! interpreter-visible deopt frame states and OSR entries (see [`super::deopt`]).
//! This backend lowers that *same* graph to Cranelift IR and finalizes it into an
//! executable [`JITModule`], letting Cranelift own instruction selection and
//! register allocation while the Otter-owned pieces — exact-PC deopt, the
//! NaN-box `Value` ABI, the frame register window as the GC root array — stay
//! exactly as the dynasm tier defines them. It is the optimizing emit choice for
//! the graphs it accepts; [`super::compile`] falls back to the dynasm
//! [`super::emit`] backend for anything this backend declines with
//! [`Unsupported`].
//!
//! A compiled artifact emits one CLIF function for the graph's entry. The
//! builder ([`super::builder`]) makes function-entry and OSR uniform: for an OSR
//! build (`osr_pc = Some`) it roots the graph at a synthetic entry block that
//! seeds *every* interpreter register as a `Param`, so the loop-header reload is
//! just the ordinary `Param`-from-frame-window lowering plus the `Check*` guards.
//! Thus the same lowering serves both:
//! - a `None` build is reached via [`JitFunctionCode::run_entry`] at its entry;
//! - an `osr_pc = Some(pc)` build is reached via
//!   [`JitFunctionCode::osr_entry`]`(pc)` at the same entry (the synthetic block
//!   loads each register from the frame window and joins the loop header).
//!
//! A `None` build whose graph carries a `Deopt` terminator — an un-compilable
//! prologue/epilogue around a hot loop — is OSR-only: a function-entry call bails
//! at PC 0 and the hot loop tiers up through a separate `osr_pc` build.
//!
//! Every guard lowers to a cold side-exit block that re-boxes the live registers
//! into the frame window, stamps the resume byte-PC, and returns
//! `Bailed(byte_pc)` — the identical interpreter-resume contract the dynasm tier
//! uses (see [`super::deopt`] and [`crate::baseline::enter_compiled`]).
//!
//! # Contents
//! - [`emit`] — graph → [`CraneliftCode`] (or [`Unsupported`] for fallback).
//! - [`CraneliftCode`] — the finalized module and its entry/OSR addresses.
//!
//! # Invariants
//! - The numeric subset has no allocating or re-entrant JS calls. It does emit a
//!   leaf/no-alloc backedge poll for interrupts and runtime budgets; Cranelift
//!   preserves live SSA values around that host-ABI call. No GC safepoint or
//!   stackmap is required. Boxed `Value`s live only in the frame window (which
//!   the VM traces); unboxed numbers and boxed-double bit patterns are
//!   non-pointers (CRANELIFT_TIER2.md §6).
//! - Finalized code is immutable; [`CraneliftCode`] captures raw entry addresses
//!   at construction and never mutates the module afterward, which is what makes
//!   sharing it across threads sound.
//!
//! # See also
//! - [`super::ir`] — the typed SSA graph lowered here.
//! - [`super::deopt`] — frame states and OSR entries consumed verbatim.
//! - `CRANELIFT_TIER2.md` — the staged backend plan this implements (Stage S0).

mod abi;
mod deopt;
mod lower;
mod runtime;

use cranelift_codegen::settings::{self, Configurable};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::default_libcall_names;
use rustc_hash::FxHashMap;

use otter_vm::JitFunctionView;

use super::Unsupported;
use super::deopt::DeoptPoint;
use super::ir::{Graph, NodeId, NodeKind, Terminator};
use crate::optimizing::ir::BlockId;

/// Finalized Cranelift code for one optimizing-tier compile.
///
/// Owns the [`JITModule`] (keeping its executable mapping alive) and the raw
/// entry addresses captured after finalization. Runs through the shared
/// [`crate::baseline::enter_compiled`], inheriting the exact reentry ABI and
/// deopt-resume handling.
pub(in crate::optimizing) struct CraneliftCode {
    /// The finalized module. Kept alive so its executable mapping outlives every
    /// call; never mutated after construction.
    _module: JITModule,
    /// Function-entry address, or `None` when the artifact is OSR-only (the graph
    /// has a `Deopt` terminator, so the top of the function runs the interpreter).
    entry_addr: Option<usize>,
    /// Loop-header byte-PC → that header's OSR trampoline entry address.
    osr_addrs: FxHashMap<u32, usize>,
    /// Total finalized code size across the artifact's functions, in bytes.
    code_len: usize,
}

// SAFETY: after `finalize_definitions` the module's code is immutable and the
// captured entry addresses point into a stable executable mapping owned by
// `_module`. `CraneliftCode` exposes only reads of that immutable code (it never
// re-enters the module), so concurrent shared access is data-race free — the same
// contract the dynasm `OptimizedCode` upholds over its `CompiledCode` mapping.
unsafe impl Send for CraneliftCode {}
// SAFETY: see the `Send` justification above.
unsafe impl Sync for CraneliftCode {}

impl std::fmt::Debug for CraneliftCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CraneliftCode")
            .field("entry", &self.entry_addr.map(|a| a as *const u8))
            .field("osr_entries", &self.osr_addrs.len())
            .field("code_len", &self.code_len)
            .finish()
    }
}

impl otter_vm::JitFunctionCode for CraneliftCode {
    fn code_len(&self) -> usize {
        self.code_len
    }

    fn osr_only(&self) -> bool {
        self.entry_addr.is_none()
    }

    fn entry_addr(&self) -> Option<usize> {
        self.entry_addr
    }

    fn run_entry(&self, ptrs: otter_vm::JitReentryPtrs) -> otter_vm::JitExecOutcome {
        match self.entry_addr {
            // An OSR-only artifact: never run the un-compilable prologue from the
            // top. Bailing at PC 0 leaves the frame untouched and runs the
            // function on the interpreter, which OSRs the hot loop on a backedge.
            None => otter_vm::JitExecOutcome::Bailed(0),
            Some(addr) => {
                // SAFETY: `addr` is a finalized entry in the live `_module` mapping
                // emitted with the shared `JitEntry` ABI; `ptrs` upholds the
                // reentry contract.
                unsafe {
                    crate::baseline::enter_compiled(ptrs, addr as *const u8, std::ptr::null(), 0)
                }
            }
        }
    }

    fn osr_entry(
        &self,
        ptrs: otter_vm::JitReentryPtrs,
        byte_pc: u32,
    ) -> Option<otter_vm::JitExecOutcome> {
        let addr = *self.osr_addrs.get(&byte_pc)?;
        // SAFETY: `addr` is a finalized OSR trampoline in the live `_module`
        // mapping; the trampoline reloads the live interpreter registers before
        // joining the loop header, upholding the same reentry contract.
        Some(unsafe {
            crate::baseline::enter_compiled(ptrs, addr as *const u8, std::ptr::null(), 0)
        })
    }
}

/// Reject a graph that uses any node kind outside the Stage S0 numeric subset,
/// so [`super::compile`] falls back to the dynasm backend. Coverage widens by
/// adding kinds here and in [`lower`]; the *algorithm* (exact-PC deopt, real
/// regalloc via Cranelift) is never weakened.
fn check_supported(graph: &Graph) -> Result<(), Unsupported> {
    for node in &graph.nodes {
        let ok = matches!(
            node.kind,
            NodeKind::Param(_)
                | NodeKind::ConstInt32(_)
                | NodeKind::ConstF64(_)
                | NodeKind::ConstBool(_)
                | NodeKind::ConstUndefined
                | NodeKind::ConstNull
                | NodeKind::Phi(_)
                | NodeKind::CheckInt32(_)
                | NodeKind::CheckNumber(_)
                | NodeKind::Int32ToFloat64(_)
                | NodeKind::Float64ToInt32(_)
                | NodeKind::Int32Add(_, _)
                | NodeKind::Int32Sub(_, _)
                | NodeKind::Int32Mul(_, _)
                | NodeKind::Int32Compare(_, _, _)
                | NodeKind::Int32BitOr(_, _)
                | NodeKind::Int32BitAnd(_, _)
                | NodeKind::Int32BitXor(_, _)
                | NodeKind::Int32Shl(_, _)
                | NodeKind::Int32Shr(_, _)
                | NodeKind::Int32UshrToFloat64(_, _)
                | NodeKind::Float64Add(_, _)
                | NodeKind::Float64Sub(_, _)
                | NodeKind::Float64Mul(_, _)
                | NodeKind::Float64Div(_, _)
                | NodeKind::Float64Compare(_, _, _)
                | NodeKind::TaggedIsNull { .. }
                | NodeKind::LoadElement(_, _)
                | NodeKind::StoreElement(_, _, _)
                | NodeKind::LoadArrayLength(_)
        );
        if !ok {
            return Err(Unsupported::Unlowered("clif: node kind outside subset"));
        }
    }
    Ok(())
}

/// Build the host-targeted Cranelift JIT module, or report the host is
/// unavailable as an [`Unsupported`] (so the dynasm tier still runs).
fn make_module() -> Result<JITModule, Unsupported> {
    let mut flags = settings::builder();
    // This tier compiles only hot code, so codegen quality outweighs compile
    // latency: ~233µs/function at `speed` vs ~184µs at `none` (the `JITModule`
    // mapping setup dominates either way), repaid within a few loop iterations.
    // `cranelift-jit` requires non-PIC absolute addressing.
    flags
        .set("opt_level", "speed")
        .map_err(|_| Unsupported::Unlowered("clif: flag opt_level"))?;
    flags
        .set("is_pic", "false")
        .map_err(|_| Unsupported::Unlowered("clif: flag is_pic"))?;
    let isa_builder = cranelift_native::builder()
        .map_err(|_| Unsupported::Unlowered("clif: host unsupported"))?;
    let isa = isa_builder
        .finish(settings::Flags::new(flags))
        .map_err(|_| Unsupported::Unlowered("clif: isa finish"))?;
    let builder = JITBuilder::with_isa(isa, default_libcall_names());
    Ok(JITModule::new(builder))
}

/// Lower the typed SSA `graph` to a finalized Cranelift artifact, or report the
/// [`Unsupported`] reason it is outside the Stage S0 subset (the caller then
/// falls back to the dynasm backend).
///
/// `osr_pc` mirrors the build mode: `None` is a function-entry compile reached
/// through [`CraneliftCode::run_entry`]; `Some(pc)` is the OSR build for the loop
/// header at `pc`, reached through [`CraneliftCode::osr_entry`]`(pc)`. Both lower
/// the graph's single entry the same way — the builder roots an OSR graph at a
/// synthetic, register-seeding entry block (see the module docs).
pub(in crate::optimizing) fn emit(
    view: &JitFunctionView,
    graph: &Graph,
    frames: &FxHashMap<NodeId, DeoptPoint>,
    block_deopts: &FxHashMap<BlockId, DeoptPoint>,
    osr_pc: Option<u32>,
) -> Result<CraneliftCode, Unsupported> {
    check_supported(graph)?;

    // A `Deopt` terminator means an un-compilable region exists. For a
    // function-entry build the function is then entered only through an OSR loop
    // header (a separate `osr_pc` build), so a function-entry call bails at PC 0;
    // there is nothing to compile. An OSR build always compiles its entry — the
    // synthetic register-seeding prologue is always lowerable, and any
    // un-compilable region inside the loop bails at its own byte-PC at runtime.
    let entry_is_osr_only = osr_pc.is_none()
        && graph
            .blocks
            .iter()
            .any(|b| matches!(b.term, Some(Terminator::Deopt(_))));

    if entry_is_osr_only {
        return Ok(CraneliftCode {
            _module: make_module()?,
            entry_addr: None,
            osr_addrs: FxHashMap::default(),
            code_len: 0,
        });
    }

    let mut module = make_module()?;
    let (func_id, code_len) =
        lower::compile_function(&mut module, view, graph, frames, block_deopts)?;

    module
        .finalize_definitions()
        .map_err(|_| Unsupported::Unlowered("clif: finalize"))?;
    let addr = module.get_finalized_function(func_id) as usize;

    let (entry_addr, osr_addrs) = match osr_pc {
        None => (Some(addr), FxHashMap::default()),
        Some(pc) => {
            let mut osr = FxHashMap::default();
            osr.insert(pc, addr);
            (None, osr)
        }
    };

    Ok(CraneliftCode {
        _module: module,
        entry_addr,
        osr_addrs,
        code_len,
    })
}

#[cfg(test)]
mod tests {
    //! End-to-end execution of Cranelift-lowered code. Each test builds a typed
    //! SSA graph from a synthetic function view, lowers it through [`emit`], and
    //! calls the finalized entry with a `JitCtx`-shaped buffer (offset 0 = the
    //! register-window pointer) using the shared
    //! `extern "C" fn(*mut JitCtx) -> JitRet` ABI — the real correctness contract
    //! the VM relies on: returned/bailed status, the boxed result, the exact
    //! resume PC, and the restored frame on a deopt.

    use otter_bytecode::{Op, Operand};
    use otter_vm::jit_feedback::{ARITH_FLOAT64, ARITH_INT32};
    use otter_vm::{JitFunctionCode, JitFunctionView};

    use crate::baseline::BAIL_PC_OFFSET;
    use crate::optimizing::{build_graph, deopt};

    use otter_vm::value::tag as value_tag;

    fn r(n: u16) -> Operand {
        Operand::Register(n)
    }
    fn imm(n: i32) -> Operand {
        Operand::Imm32(n)
    }
    fn boxi(v: i32) -> u64 {
        value_tag::NUMBER_TAG | (v as u32 as u64)
    }
    fn unboxi(v: u64) -> i32 {
        v as u32 as i32
    }
    fn boxf(v: f64) -> u64 {
        let bits = if v.is_nan() {
            value_tag::CANONICAL_NAN
        } else {
            v.to_bits()
        };
        value_tag::box_double(bits)
    }

    #[repr(C)]
    struct Ret {
        value: u64,
        status: u64,
    }

    fn view(
        param_count: u16,
        register_count: u16,
        instrs: &[(Op, Vec<Operand>, u8)],
    ) -> JitFunctionView {
        let instructions = instrs
            .iter()
            .enumerate()
            .map(|(idx, (op, ops, fb))| otter_vm::JitInstrView {
                op: *op,
                byte_pc: idx as u32,
                byte_len: 1,
                property_ic_site: None,
                operands: ops.clone(),
                make_self: false,
                load_array_length: false,
                load_number: None,
                property_feedback: None,
                property_feedback_poly: Vec::new(),
                property_proto_feedback: None,
                object_literal: None,
                arith_feedback: *fb,
            })
            .collect();
        JitFunctionView {
            function_id: 0,
            param_count,
            register_count,
            code_byte_len: instrs.len() as u32,
            is_strict: true,
            is_async: false,
            is_generator: false,
            is_async_generator: false,
            cage_base: 0,
            ta_layout: Default::default(),
            object_shape_byte: 8,
            object_values_ptr_byte: 16,
            object_inline_values_byte: 80,
            object_slab_len_byte: 88,
            object_inline_slot_cap: 2,
            gc_barrier: Default::default(),
            jit_proto_byte: 12,
            closure_fid_byte: 8,
            closure_upvalues_ptr_byte: 16,
            collection_layout: Default::default(),
            native_static_fn_byte: 0,
            instructions,
            inline_callees: Default::default(),
            inline_methods: Default::default(),
            inline_poly_methods: Default::default(),
            collection_leaf_methods: Default::default(),
            collection_alloc_methods: Default::default(),
            array_methods: Default::default(),
            safepoints: Default::default(),
        }
    }

    /// Lower `v` through Cranelift, run it with frame slot `i` preloaded from
    /// `params[i]`, and return `(status, boxed value, bail_pc, restored regs)`.
    fn run(v: &JitFunctionView, params: &[u64]) -> (u64, u64, u32, Vec<u64>) {
        let g = build_graph(v).expect("builds");
        let bcl = deopt::bytecode_liveness(v);
        let frames = deopt::capture_frame_states(&g, &bcl);
        let block_deopts = deopt::capture_deopt_terminators(&g, &bcl);
        let code = super::emit(v, &g, &frames, &block_deopts, None).expect("clif emits");
        let entry = code.entry_addr().expect("function-entry address");

        let mut regs = vec![0u64; 64];
        for (i, &p) in params.iter().enumerate() {
            regs[i] = p;
        }
        let mut ctx = vec![0u64; 64];
        ctx[0] = regs.as_mut_ptr() as u64;
        // SAFETY: `entry` is a finalized function emitted with the shared
        // `extern "C" fn(*mut JitCtx) -> JitRet` ABI; `ctx` is a JitCtx-shaped
        // buffer whose offset-0 register pointer is a valid 64-slot window;
        // `code` (owning the executable mapping) outlives the call.
        let f: extern "C" fn(*mut u64) -> Ret = unsafe { std::mem::transmute(entry) };
        let ret = f(ctx.as_mut_ptr());
        // SAFETY: `bail_pc` is a `u32` at `BAIL_PC_OFFSET` within the `ctx` buffer.
        let bail_pc = unsafe {
            (ctx.as_ptr() as *const u8)
                .add(BAIL_PC_OFFSET as usize)
                .cast::<u32>()
                .read_unaligned()
        };
        (ret.status, ret.value, bail_pc, regs)
    }

    /// `f(n) { return n + n }` over int32 feedback.
    fn add_self() -> JitFunctionView {
        view(
            1,
            3,
            &[
                (Op::Add, vec![r(1), r(0), r(0)], ARITH_INT32),
                (Op::ReturnValue, vec![r(1)], 0),
            ],
        )
    }

    #[test]
    fn int32_add_returns() {
        let (status, value, _, _) = run(&add_self(), &[boxi(21)]);
        assert_eq!(status, 0, "returns, no bail");
        assert_eq!(unboxi(value), 42, "21 + 21 == 42");
    }

    #[test]
    fn non_int32_operand_bails_at_exact_pc_and_restores_frame() {
        // A double param fails the `CheckInt32` guard on the `Add` at byte-pc 0.
        let (status, _, bail_pc, regs) = run(&add_self(), &[boxf(3.5)]);
        assert_eq!(status, 1, "guard miss bails");
        assert_eq!(
            bail_pc, 0,
            "resumes at the arithmetic instruction's exact PC"
        );
        assert_eq!(
            regs[0],
            boxf(3.5),
            "the live param is restored to its frame slot"
        );
    }

    #[test]
    fn int32_overflow_bails() {
        // i32::MAX + i32::MAX overflows signed int32 → deopt at the `Add` PC.
        let (status, _, bail_pc, _) = run(&add_self(), &[boxi(i32::MAX)]);
        assert_eq!(status, 1, "overflow bails");
        assert_eq!(bail_pc, 0, "resumes at the add's exact PC");
    }

    #[test]
    fn float64_to_int32_bitwise_or_returns_js_to_int32() {
        let bit_or_zero = view(
            1,
            3,
            &[
                (Op::LoadInt32, vec![r(1), imm(0)], 0),
                (
                    Op::BitwiseOr,
                    vec![r(2), r(0), r(1)],
                    ARITH_INT32 | ARITH_FLOAT64,
                ),
                (Op::ReturnValue, vec![r(2)], 0),
            ],
        );
        for (input, expected) in [
            (2_500_000.0, 2_500_000),
            (4_294_967_297.0, 1),
            (-1.5, -1),
            (f64::NAN, 0),
            (f64::INFINITY, 0),
        ] {
            let (status, value, _, _) = run(&bit_or_zero, &[boxf(input)]);
            assert_eq!(status, 0, "{input:?} returns, no bail");
            assert_eq!(unboxi(value), expected, "ToInt32({input:?})");
        }
    }

    /// `f(n){ i=0; acc=0; while (i<n){ acc+=i; i+=1 } return acc }` — a counting
    /// loop whose induction and accumulator are loop-carried phis (block params).
    fn counting_loop() -> JitFunctionView {
        view(
            1,
            5,
            &[
                (Op::LoadInt32, vec![r(1), imm(0)], 0),
                (Op::LoadInt32, vec![r(2), imm(0)], 0),
                (Op::LessThan, vec![r(3), r(1), r(0)], ARITH_INT32),
                (Op::JumpIfFalse, vec![imm(4), r(3)], 0),
                (Op::Add, vec![r(2), r(2), r(1)], ARITH_INT32),
                (Op::LoadInt32, vec![r(4), imm(1)], 0),
                (Op::Add, vec![r(1), r(1), r(4)], ARITH_INT32),
                (Op::Jump, vec![imm(-6)], 0),
                (Op::ReturnValue, vec![r(2)], 0),
            ],
        )
    }

    #[test]
    fn counting_loop_sums_through_phis() {
        // sum(0..5) = 10; sum(0..100) = 4950.
        let (s, v, _, _) = run(&counting_loop(), &[boxi(5)]);
        assert_eq!(s, 0);
        assert_eq!(unboxi(v), 10);
        let (s, v, _, _) = run(&counting_loop(), &[boxi(100)]);
        assert_eq!(s, 0);
        assert_eq!(unboxi(v), 4950);
    }

    #[test]
    fn nan_box_canonical_constant_matches_dynasm() {
        // Boxing any NaN canonicalizes to `CANONICAL_NAN`, then the encode offset
        // is applied, so both backends store identical bits for a NaN result.
        assert_eq!(
            boxf(f64::NAN),
            value_tag::box_double(value_tag::CANONICAL_NAN)
        );
    }

    /// Report Cranelift compile time per function (the risk CRANELIFT_TIER2.md §9
    /// flags). Run with `cargo test -p otter-jit --release -- --ignored
    /// --nocapture compile_time_per_function`. The measured `emit` includes
    /// `JITModule` creation, lowering, and finalization — the full per-function
    /// cost the tier-up path pays.
    #[test]
    #[ignore = "timing report, not a correctness assertion"]
    fn compile_time_per_function() {
        let v = counting_loop();
        let g = build_graph(&v).expect("builds");
        let bcl = deopt::bytecode_liveness(&v);
        let frames = deopt::capture_frame_states(&g, &bcl);
        let block_deopts = deopt::capture_deopt_terminators(&g, &bcl);

        // Warm up, then time a batch.
        for _ in 0..10 {
            let _ = super::emit(&v, &g, &frames, &block_deopts, None).expect("emits");
        }
        let iters = 200;
        let start = std::time::Instant::now();
        for _ in 0..iters {
            let _ = super::emit(&v, &g, &frames, &block_deopts, None).expect("emits");
        }
        let per = start.elapsed() / iters;
        eprintln!("clif compile_time_per_function (counting loop): {per:?}");
    }
}
