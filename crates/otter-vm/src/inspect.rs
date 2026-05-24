//! Step tracing observer for the VM dispatch loop.
//!
//! Per-instruction step trace: every dispatched instruction produces
//! one canonical line of text describing the frame, the byte-offset PC,
//! the opcode mnemonic, and the operand list. Embedders install a
//! [`StepTracer`] once and the dispatch loop walks every instruction
//! through it. Off-state cost is one branch on a `None` slot.
//!
//! # Contents
//! - [`StepTracer`] — observer trait implemented by the embedder.
//! - [`StepEvent`] — per-instruction event payload.
//! - [`format_event`] / [`format_header`] — canonical text writers.
//! - [`WriterTracer`] — flushes one line per event to a `Write` sink.
//! - [`TRACE_FORMAT_VERSION`] — banner emitted ahead of every trace
//!   stream; bumped on incompatible format changes.
//!
//! # Invariants
//! - Mnemonics come from [`otter_bytecode::Op::mnemonic`]; renaming an
//!   opcode shifts every golden trace at compile time through the
//!   shared mnemonic table.
//! - The hot dispatch path checks one `Option` slot per instruction
//!   and pays no allocation when the slot is `None`.
//! - The format is line-oriented and deterministic given a fixed
//!   bytecode module and runtime configuration.
//!
//! # See also
//! - `crate::Interpreter::set_tracer`
//! - [`otter_bytecode::disasm`]
//! - `docs/book/src/engine/step-trace.md`

use std::fmt::Write as _;
use std::io::Write;

use otter_bytecode::{Op, Operand};

/// Canonical version banner. Bump on any format change that breaks
/// existing golden traces.
pub const TRACE_FORMAT_VERSION: &str = "otter step trace v1";

/// Per-instruction trace payload. Borrowed from the dispatch loop;
/// implementations must not retain references past
/// [`StepTracer::on_step`].
#[derive(Debug, Clone, Copy)]
pub struct StepEvent<'a> {
    /// 1-based call depth (number of frames on the dispatch stack at
    /// the moment the instruction begins executing). The active
    /// frame is at depth `frame_depth - 1`.
    pub frame_depth: usize,
    /// VM-local function id of the active frame.
    pub function_id: u32,
    /// Source-declared function name. `<main>` for module entry.
    pub function_name: &'a str,
    /// Byte-offset PC of the instruction inside the function's
    /// encoded stream.
    pub byte_pc: u32,
    /// Opcode about to dispatch. Mnemonic resolved through
    /// [`Op::mnemonic`].
    pub op: Op,
    /// Operands in declaration order.
    pub operands: &'a [Operand],
}

/// VM dispatch observer.
///
/// One method per observable transition. Default methods exist so
/// embedders can implement only the events they care about.
pub trait StepTracer {
    /// Fires once for every dispatched instruction, right before the
    /// opcode body runs. Frame depth, PC, and operands describe the
    /// state immediately before dispatch.
    fn on_step(&mut self, event: &StepEvent<'_>);
}

/// Convenience writer-backed tracer. Emits one line per event using
/// [`format_event`] and the trace banner from [`format_header`].
pub struct WriterTracer<W: Write> {
    writer: W,
    wrote_header: bool,
    buf: String,
}

impl<W: Write> WriterTracer<W> {
    /// Wrap `writer`. The header banner is written lazily on the
    /// first event so callers can install a tracer before run-start
    /// without paying for a flush.
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            wrote_header: false,
            buf: String::with_capacity(96),
        }
    }

    /// Surrender the inner writer. Useful for tests that want to
    /// inspect the captured text.
    pub fn into_inner(self) -> W {
        self.writer
    }

    fn ensure_header(&mut self) -> std::io::Result<()> {
        if !self.wrote_header {
            self.wrote_header = true;
            self.buf.clear();
            format_header(&mut self.buf);
            self.buf.push('\n');
            self.writer.write_all(self.buf.as_bytes())?;
        }
        Ok(())
    }
}

impl<W: Write> StepTracer for WriterTracer<W> {
    fn on_step(&mut self, event: &StepEvent<'_>) {
        if self.ensure_header().is_err() {
            return;
        }
        self.buf.clear();
        format_event(&mut self.buf, event);
        self.buf.push('\n');
        let _ = self.writer.write_all(self.buf.as_bytes());
    }
}

/// Write the canonical banner.
pub fn format_header(out: &mut String) {
    out.push_str("; ");
    out.push_str(TRACE_FORMAT_VERSION);
}

/// Append the canonical text form of one [`StepEvent`].
///
/// Format: `frame=<depth> fn=<name> pc=<6-digit byte pc> op=<MNEMONIC> [operands...]`.
pub fn format_event(out: &mut String, event: &StepEvent<'_>) {
    let _ = write!(
        out,
        "frame={} fn={} pc={:06} op={}",
        event.frame_depth,
        event.function_name,
        event.byte_pc,
        event.op.mnemonic(),
    );
    if !event.operands.is_empty() {
        out.push_str("  ");
        let mut first = true;
        for operand in event.operands {
            if !first {
                out.push(' ');
            }
            first = false;
            format_operand(out, operand);
        }
    }
}

fn format_operand(out: &mut String, operand: &Operand) {
    match operand {
        Operand::Register(r) => {
            let _ = write!(out, "r{r}");
        }
        Operand::ConstIndex(k) => {
            let _ = write!(out, "k[{k}]");
        }
        Operand::Imm32(v) => {
            let _ = write!(out, "i32:{v}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_text_is_versioned() {
        let mut out = String::new();
        format_header(&mut out);
        assert_eq!(out, "; otter step trace v1");
    }

    #[test]
    fn single_event_renders_canonical_line() {
        let operands = [Operand::Register(2), Operand::Register(0), Operand::Register(1)];
        let event = StepEvent {
            frame_depth: 1,
            function_id: 0,
            function_name: "<main>",
            byte_pc: 12,
            op: Op::Add,
            operands: &operands,
        };
        let mut out = String::new();
        format_event(&mut out, &event);
        assert_eq!(out, "frame=1 fn=<main> pc=000012 op=ADD  r2 r0 r1");
    }

    #[test]
    fn writer_tracer_emits_header_then_lines() {
        let mut buf: Vec<u8> = Vec::new();
        {
            let mut tracer = WriterTracer::new(&mut buf);
            let operands = [Operand::Register(0), Operand::Imm32(7)];
            let event = StepEvent {
                frame_depth: 1,
                function_id: 0,
                function_name: "<main>",
                byte_pc: 0,
                op: Op::LoadInt32,
                operands: &operands,
            };
            tracer.on_step(&event);
            tracer.on_step(&event);
        }
        let text = String::from_utf8(buf).expect("utf-8");
        let mut lines = text.lines();
        assert_eq!(lines.next(), Some("; otter step trace v1"));
        assert_eq!(
            lines.next(),
            Some("frame=1 fn=<main> pc=000000 op=LOAD_INT32  r0 i32:7")
        );
        assert_eq!(
            lines.next(),
            Some("frame=1 fn=<main> pc=000000 op=LOAD_INT32  r0 i32:7")
        );
    }

    /// Schema guard: every Op variant resolves to a non-empty,
    /// unique mnemonic. Walks [`otter_bytecode::encoding::OP_BYTE_TABLE`]
    /// which is the authoritative dense table for the bytecode wire
    /// format — adding, removing, or renaming an opcode shows up
    /// here on the same change that updates the wire format, so
    /// goldens recorded against the trace cannot silently desync
    /// from the opcode set.
    #[test]
    fn every_table_op_has_unique_mnemonic() {
        use std::collections::HashSet;
        let mut seen: HashSet<&'static str> = HashSet::new();
        for (op, _byte) in otter_bytecode::encoding::OP_BYTE_TABLE {
            let m = op.mnemonic();
            assert!(!m.is_empty(), "{op:?} has empty mnemonic");
            assert!(seen.insert(m), "duplicate mnemonic {m} on {op:?}");
        }
    }

    /// Per-call schema gate: every value reachable through
    /// [`otter_bytecode::encoding::OP_BYTE_TABLE`] is also enumerable
    /// from this fixed reference list. Adding a new Op variant to
    /// the wire format without listing it here fails the round-trip
    /// — the goldens then point at the missing variant before they
    /// shift in unrelated places.
    #[test]
    fn op_table_matches_reference_list() {
        use std::collections::HashSet;
        let table: HashSet<Op> = otter_bytecode::encoding::OP_BYTE_TABLE
            .iter()
            .map(|(op, _)| *op)
            .collect();
        let reference: HashSet<Op> = ALL_OPS.iter().copied().collect();
        let missing_in_reference: Vec<_> = table.difference(&reference).copied().collect();
        let missing_in_table: Vec<_> = reference.difference(&table).copied().collect();
        assert!(
            missing_in_reference.is_empty() && missing_in_table.is_empty(),
            "Op enum drift: missing_in_reference={missing_in_reference:?} missing_in_table={missing_in_table:?}",
        );
    }

    // Reference Op list. Any new Op variant must be added here AND
    // to `OP_BYTE_TABLE`. This dual-listing keeps the trace schema
    // visible in the inspect module so a future reviewer cannot
    // ship a new opcode without revisiting the trace surface.
    const ALL_OPS: &[Op] = &[
        Op::Nop,
        Op::LoadUndefined,
        Op::LoadHole,
        Op::Return,
        Op::LoadString,
        Op::LoadNumber,
        Op::LoadInt32,
        Op::LoadBigInt,
        Op::LoadRegExp,
        Op::QueueMicrotask,
        Op::PromiseNew,
        Op::PromiseCall,
        Op::LoadTrue,
        Op::LoadFalse,
        Op::LoadLength,
        Op::GetStringIndex,
        Op::CallMethodValue,
        Op::Add,
        Op::Sub,
        Op::Mul,
        Op::Div,
        Op::Rem,
        Op::Neg,
        Op::Pow,
        Op::BitwiseAnd,
        Op::BitwiseOr,
        Op::BitwiseXor,
        Op::BitwiseNot,
        Op::Shl,
        Op::Shr,
        Op::Ushr,
        Op::ToNumber,
        Op::Equal,
        Op::NotEqual,
        Op::LessThan,
        Op::LessEq,
        Op::GreaterThan,
        Op::GreaterEq,
        Op::LoadNull,
        Op::LogicalNot,
        Op::ToBoolean,
        Op::Jump,
        Op::JumpIfTrue,
        Op::JumpIfFalse,
        Op::JumpIfNullish,
        Op::LoadLocal,
        Op::StoreLocal,
        Op::TdzError,
        Op::MakeFunction,
        Op::MakeClosure,
        Op::LoadUpvalue,
        Op::StoreUpvalue,
        Op::Call,
        Op::CallWithThis,
        Op::BindFunction,
        Op::LoadThis,
        Op::LoadNewTarget,
        Op::Throw,
        Op::EnterTry,
        Op::LeaveTry,
        Op::EndFinally,
        Op::NewError,
        Op::GetIterator,
        Op::IteratorNext,
        Op::ArrayPush,
        Op::CallSpread,
        Op::New,
        Op::NewSpread,
        Op::SuperConstructSpread,
        Op::MakeClass,
        Op::MathLoad,
        Op::CollectRest,
        Op::ReturnValue,
        Op::ReturnUndefined,
        Op::NewObject,
        Op::LoadProperty,
        Op::StoreProperty,
        Op::DeleteProperty,
        Op::GetPrototype,
        Op::SetPrototype,
        Op::NewArray,
        Op::LoadElement,
        Op::StoreElement,
        Op::ArrayLength,
        Op::HasProperty,
        Op::Instanceof,
        Op::Eval,
        Op::NewFunction,
        Op::LoadGlobalThis,
        Op::LoadGlobalOrThrow,
        Op::CollectArguments,
        Op::LoadGlobalOrUndefined,
        Op::ImportMetaResolve,
        Op::ImportNamespaceDynamic,
        Op::ImportNamespace,
        Op::PromiseFulfilledOf,
        Op::NewIntl,
        Op::TemporalLoad,
        Op::NewCollection,
        Op::NewWeakRef,
        Op::NewFinalizationRegistry,
        Op::SymbolLoad,
        Op::TypeOf,
        Op::DeleteElement,
        Op::Await,
        Op::SameValue,
        Op::IsArray,
        Op::LooseEqual,
        Op::LooseNotEqual,
        Op::NewBuiltinError,
        Op::LoadBuiltinError,
        Op::BigIntCall,
        Op::ArrayConstruct,
        Op::ArrayFrom,
        Op::ArrayOf,
        Op::ArrayBufferCall,
        Op::DataViewCall,
        Op::Yield,
        Op::SharedArrayBufferCall,
        Op::ToPrimitive,
        Op::ForInKeys,
        Op::CopyDataProperties,
        Op::DefineOwnProperty,
        Op::DefineGlobalVar,
    ];
}
