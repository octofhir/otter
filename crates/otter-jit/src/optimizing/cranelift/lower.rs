//! Cranelift lowering for restartable, side-effect-free Number leaves.
//!
//! # Contents
//! - [`lower`] — builds CLIF, invokes the native Cranelift backend, and returns
//!   unrelocated machine bytes.
//! - Entry Number decoding for Otter's frozen tagged-int / encoded-double ABI.
//! - Canonical JavaScript Number boxing and the two-word `JitRet` return.
//!
//! # Invariants
//! - The generated function has the existing
//!   `extern "C" fn(*mut JitCtx) -> JitRet` ABI.
//! - A non-Number parameter writes logical PC zero and returns `BAILED` before
//!   any frame slot or externally visible state changes.
//! - Successful execution is call-free and safepoint-free.
//! - Machine output containing an external relocation, trap, call site, or
//!   user stack map is rejected before it reaches executable memory.
//!
//! # See also
//! - [`super::plan`] proves the accepted bytecode subset.
//! - `crate::code::CompiledCode` remains the sole W^X allocation owner.

use std::fmt::Write as _;

use cranelift_codegen::{
    Context,
    control::ControlPlane,
    ir::{
        AbiParam, Function, InstBuilder, MemFlagsData, Signature, SourceLoc, UserFuncName, Value,
        condcodes::{FloatCC, IntCC},
        types,
    },
    isa::{TargetIsa, unwind::UnwindInst},
};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};

use crate::entry::{
    CANONICAL_NAN_HI16, DOUBLE_OFFSET_HI16, NATIVE_FRAME_OFFSET, NATIVE_FRAME_PC_OFFSET,
    NATIVE_FRAME_REGISTER_BASE_OFFSET, NUMBER_TAG_HI16, STATUS_BAILED, STATUS_RETURNED,
};

use super::plan::{NumericLeafPlan, NumericNode, NumericSource};

pub(super) struct NumericSourceRange {
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) source: NumericSource,
}

pub(super) struct LoweredNumericLeaf {
    pub(super) bytes: Vec<u8>,
    pub(super) generated_stack_frame_bytes: u32,
    pub(super) tier_input: Option<String>,
    pub(super) source_ranges: Vec<NumericSourceRange>,
}

pub(super) fn lower(
    plan: &NumericLeafPlan,
    function_id: u32,
    isa: &dyn TargetIsa,
    capture_ir: bool,
) -> Option<LoweredNumericLeaf> {
    let mut signature = Signature::new(isa.default_call_conv());
    signature.params.push(AbiParam::new(types::I64));
    signature.returns.push(AbiParam::new(types::I64));
    signature.returns.push(AbiParam::new(types::I64));
    let function = Function::with_name_signature(UserFuncName::user(0, function_id), signature);
    let mut context = Context::for_function(function);
    let mut builder_context = FunctionBuilderContext::new();

    {
        let mut builder = FunctionBuilder::new(&mut context.func, &mut builder_context);
        build_function(&mut builder, plan, capture_ir)?;
        builder.seal_all_blocks();
        builder.finalize();
    }

    let tier_input = capture_ir.then(|| {
        let mut output = String::from("; backend=cranelift numeric-leaf\n");
        writeln!(
            output,
            "; parameters={} registers={} arithmetic-ops={}",
            plan.parameter_count, plan.register_count, plan.arithmetic_op_count
        )
        .expect("writing to String cannot fail");
        write!(output, "{}", context.func.display()).expect("writing to String cannot fail");
        output
    });

    let mut control_plane = ControlPlane::default();
    let compiled = context.compile(isa, &mut control_plane).ok()?;
    if !compiled.buffer.relocs().is_empty()
        || !compiled.buffer.traps().is_empty()
        || !compiled.buffer.user_stack_maps().is_empty()
        || compiled.buffer.call_sites().next().is_some()
        || compiled.buffer.patchable_call_sites().next().is_some()
    {
        return None;
    }
    // Finalized frame metadata reports every byte below FP, including spills
    // and saved callee registers. The unwind stream independently publishes
    // the exact FP/LR setup reservation above it. Reject anything except one
    // proven frame setup rather than assuming a backend prologue shape.
    let mut setup_bytes = None;
    for (_, instruction) in &compiled.buffer.unwind_info {
        if let UnwindInst::PushFrameRegs {
            offset_upward_to_caller_sp,
        } = instruction
        {
            if setup_bytes.replace(*offset_upward_to_caller_sp).is_some() {
                return None;
            }
        }
    }
    let setup_bytes = setup_bytes?;
    let generated_stack_frame_bytes = compiled
        .buffer
        .frame_layout()?
        .frame_to_fp_offset
        .checked_add(setup_bytes)?;
    if setup_bytes == 0 || !generated_stack_frame_bytes.is_multiple_of(16) {
        return None;
    }
    let bytes = compiled.code_buffer().to_vec();
    if bytes.is_empty() || !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut source_ranges = Vec::<NumericSourceRange>::new();
    if capture_ir {
        for range in compiled.buffer.get_srclocs_sorted() {
            let Some(source) = plan.source_for_logical_pc(range.loc.bits()).cloned() else {
                continue;
            };
            let start = usize::try_from(range.start).ok()?;
            let end = usize::try_from(range.end).ok()?;
            if start >= end || end > bytes.len() {
                return None;
            }
            if let Some(previous) = source_ranges.last_mut() {
                if previous.source == source {
                    // Cranelift may split one SourceLoc around backend-owned
                    // instructions. Attribute the whole uninterrupted source
                    // interval to the same bytecode operation so the assembly
                    // map never exposes a false semantic boundary.
                    previous.end = previous.end.max(end);
                    continue;
                }
                if start < previous.end {
                    return None;
                }
            }
            source_ranges.push(NumericSourceRange { start, end, source });
        }
    }
    Some(LoweredNumericLeaf {
        bytes,
        generated_stack_frame_bytes,
        tier_input,
        source_ranges,
    })
}

fn build_function(
    builder: &mut FunctionBuilder<'_>,
    plan: &NumericLeafPlan,
    capture_sources: bool,
) -> Option<()> {
    let entry = builder.create_block();
    let bail = builder.create_block();
    builder.switch_to_block(entry);
    builder.append_block_params_for_function_params(entry);
    let context_ptr = *builder.block_params(entry).first()?;
    let trusted = MemFlagsData::trusted();
    let native_frame = builder.ins().load(
        types::I64,
        trusted,
        context_ptr,
        i32::try_from(NATIVE_FRAME_OFFSET).ok()?,
    );
    let register_base = builder.ins().load(
        types::I64,
        trusted,
        native_frame,
        i32::try_from(NATIVE_FRAME_REGISTER_BASE_OFFSET).ok()?,
    );

    let mut parameters = Vec::with_capacity(usize::from(plan.parameter_count));
    for parameter in 0..plan.parameter_count {
        let offset = i32::from(parameter).checked_mul(8)?;
        let bits = builder
            .ins()
            .load(types::I64, trusted, register_base, offset);
        parameters.push(decode_number(builder, bits, bail));
    }

    let mut values = vec![None; plan.nodes.len()];
    for (index, node) in plan.nodes.iter().copied().enumerate() {
        set_source(
            builder,
            capture_sources
                .then(|| plan.node_sources[index].as_ref())
                .flatten(),
        );
        let value = match node {
            NumericNode::Parameter(parameter) => *parameters.get(usize::from(parameter))?,
            NumericNode::Constant(value) => {
                // Materialize IEEE-754 bits through integer immediates. The
                // AArch64 backend otherwise prefers a PC-relative literal
                // pool, which violates Otter's address-free artifact
                // invariant even though the generated code is executable.
                let bits = builder.ins().iconst(types::I64, value.to_bits() as i64);
                builder.ins().bitcast(types::F64, MemFlagsData::new(), bits)
            }
            NumericNode::Add(left, right) => builder
                .ins()
                .fadd(node_value(&values, left)?, node_value(&values, right)?),
            NumericNode::Sub(left, right) => builder
                .ins()
                .fsub(node_value(&values, left)?, node_value(&values, right)?),
            NumericNode::Mul(left, right) => builder
                .ins()
                .fmul(node_value(&values, left)?, node_value(&values, right)?),
            NumericNode::Div(left, right) => builder
                .ins()
                .fdiv(node_value(&values, left)?, node_value(&values, right)?),
            NumericNode::Neg(source) => builder.ins().fneg(node_value(&values, source)?),
        };
        values[index] = Some(value);
    }

    set_source(builder, capture_sources.then_some(&plan.return_source));
    let result = node_value(&values, plan.result)?;
    let is_nan = builder.ins().fcmp(FloatCC::Unordered, result, result);
    let raw_bits = builder
        .ins()
        .bitcast(types::I64, MemFlagsData::new(), result);
    let canonical_nan = builder
        .ins()
        .iconst(types::I64, ((u64::from(CANONICAL_NAN_HI16)) << 48) as i64);
    let raw_bits = builder.ins().select(is_nan, canonical_nan, raw_bits);
    let double_offset = builder
        .ins()
        .iconst(types::I64, ((u64::from(DOUBLE_OFFSET_HI16)) << 48) as i64);
    let boxed_double = builder.ins().iadd(raw_bits, double_offset);

    // Match the VM's canonical Number representation at the tier boundary:
    // exact i32 values return as tagged ints, while -0, NaN, infinities, and
    // out-of-range integers remain encoded doubles.
    let integer = builder.ins().fcvt_to_sint_sat(types::I32, result);
    let roundtrip = builder.ins().fcvt_from_sint(types::F64, integer);
    let is_exact_integer = builder.ins().fcmp(FloatCC::Equal, result, roundtrip);
    let negative_zero_bits = builder
        .ins()
        .iconst(types::I64, (-0.0_f64).to_bits() as i64);
    let is_negative_zero = builder
        .ins()
        .icmp(IntCC::Equal, raw_bits, negative_zero_bits);
    let is_not_negative_zero = builder.ins().bnot(is_negative_zero);
    let use_tagged_int = builder.ins().band(is_exact_integer, is_not_negative_zero);
    let integer_payload = builder.ins().uextend(types::I64, integer);
    let number_tag = builder
        .ins()
        .iconst(types::I64, ((u64::from(NUMBER_TAG_HI16)) << 48) as i64);
    let boxed_integer = builder.ins().bor(number_tag, integer_payload);
    let boxed = builder
        .ins()
        .select(use_tagged_int, boxed_integer, boxed_double);
    let returned = builder.ins().iconst(types::I64, STATUS_RETURNED as i64);
    builder.ins().return_(&[boxed, returned]);

    builder.switch_to_block(bail);
    set_source(builder, None);
    let zero = builder.ins().iconst(types::I32, 0);
    builder.ins().store(
        trusted,
        zero,
        native_frame,
        i32::try_from(NATIVE_FRAME_PC_OFFSET).ok()?,
    );
    let empty = builder.ins().iconst(types::I64, 0);
    let bailed = builder.ins().iconst(types::I64, STATUS_BAILED as i64);
    builder.ins().return_(&[empty, bailed]);
    Some(())
}

fn set_source(builder: &mut FunctionBuilder<'_>, source: Option<&NumericSource>) {
    builder.set_srcloc(source.map_or_else(SourceLoc::default, |source| {
        SourceLoc::new(source.logical_pc)
    }));
}

fn decode_number(
    builder: &mut FunctionBuilder<'_>,
    bits: Value,
    bail: cranelift_codegen::ir::Block,
) -> Value {
    let int_path = builder.create_block();
    let non_int_path = builder.create_block();
    let double_path = builder.create_block();
    let join = builder.create_block();
    builder.append_block_param(join, types::F64);

    let tag = builder
        .ins()
        .iconst(types::I64, ((u64::from(NUMBER_TAG_HI16)) << 48) as i64);
    let masked = builder.ins().band(bits, tag);
    let is_int = builder.ins().icmp(IntCC::Equal, masked, tag);
    builder.ins().brif(is_int, int_path, &[], non_int_path, &[]);

    builder.switch_to_block(int_path);
    let integer = builder.ins().ireduce(types::I32, bits);
    let integer = builder.ins().fcvt_from_sint(types::F64, integer);
    builder.ins().jump(join, &[integer.into()]);

    builder.switch_to_block(non_int_path);
    let zero = builder.ins().iconst(types::I64, 0);
    let is_non_number = builder.ins().icmp(IntCC::Equal, masked, zero);
    builder
        .ins()
        .brif(is_non_number, bail, &[], double_path, &[]);

    builder.switch_to_block(double_path);
    let double_offset = builder
        .ins()
        .iconst(types::I64, ((u64::from(DOUBLE_OFFSET_HI16)) << 48) as i64);
    let decoded = builder.ins().isub(bits, double_offset);
    let decoded = builder
        .ins()
        .bitcast(types::F64, MemFlagsData::new(), decoded);
    builder.ins().jump(join, &[decoded.into()]);

    builder.switch_to_block(join);
    builder.block_params(join)[0]
}

fn node_value(values: &[Option<Value>], node: super::plan::NumericNodeId) -> Option<Value> {
    values.get(node.0).copied().flatten()
}
