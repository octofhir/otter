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
//!
//! # Snapshots
//! On top of the per-instruction trace, this module exposes
//! point-in-time DTOs that surface the interpreter's hot-path
//! state without leaking internal types:
//!
//! - [`IcSiteSnapshot`] / [`IcSiteState`] / [`IcEntrySnapshot`] —
//!   one entry per property inline-cache site.
//! - [`ShapeTransitionSnapshot`] / [`ShapeNodeSnapshot`] — flat
//!   walk of the hidden-class transition tree.
//! - [`ShapeTransitionObserver`] — install once on the
//!   interpreter to break on every fresh hidden-class transition
//!   (the cache-miss path; cached transition lookups never
//!   re-fire the observer).
//! - [`FrameSnapshot`] / [`RegisterSnapshot`] — frame and
//!   register window inspection from inside a step-tracer hook.

use std::fmt::Write as _;
use std::io::Write;

use otter_bytecode::{Op, Operand};

use crate::Value;

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
    /// Register window of the active frame. Embedders that want
    /// frame/register inspection from inside a tracer hook should
    /// read this slice directly — it is the same backing storage the
    /// dispatch loop sees.
    pub register_window: &'a [Value],
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

// ---------------------------------------------------------------------------
// IC, shape, and frame snapshots — point-in-time dumps for inspector tooling.
// ---------------------------------------------------------------------------

/// Family of named-property bytecodes a polymorphic inline cache
/// belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IcSiteKind {
    /// `LoadProperty` site.
    Load,
    /// `StoreProperty` site.
    Store,
    /// `HasProperty` site.
    Has,
}

/// Lifecycle state of one inline-cache site.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IcSiteState {
    /// No cache record has been installed yet.
    Empty,
    /// One or more guarded entries are installed. `misses` is the
    /// running probe-miss budget; reaching the disable threshold
    /// while the entry list is full transitions the site to
    /// [`Self::Megamorphic`].
    Polymorphic {
        /// Installed entries in install order.
        entries: Vec<IcEntrySnapshot>,
        /// Probe misses observed since the most recent install.
        misses: u32,
    },
    /// Site exceeded the polymorphic cache budget and falls through
    /// to the slow path on every dispatch.
    Megamorphic,
}

/// One entry inside a polymorphic IC.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IcEntrySnapshot {
    /// Whether the cache record fires on an own slot, a direct
    /// prototype slot, or a hidden-class transition.
    pub variant: IcEntryVariant,
    /// Guarded receiver shape id rendered as `u64`. Distinct between
    /// any two shape histories.
    pub receiver_shape_id: u64,
    /// Guarded property key when known. Resolved through the cached
    /// constant pool / shape key table; rendered as UTF-8.
    pub key: Option<String>,
    /// Property slot offset on the matched shape.
    pub slot: Option<u16>,
    /// For transition entries: the destination shape id.
    pub to_shape_id: Option<u64>,
}

/// Inline-cache record family inside a [`IcEntrySnapshot`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IcEntryVariant {
    /// Receiver owns the matched data slot.
    OwnData,
    /// Receiver's direct prototype owns the matched data slot.
    DirectPrototypeData,
    /// Append transition that adds a slot on store.
    OwnAddTransition,
    /// Store transition guarded by a direct-prototype miss.
    DirectPrototypeMissingTransition,
    /// Store transition guarded by a direct-prototype writable
    /// data slot.
    DirectPrototypeWritableDataTransition,
}

/// One inline-cache site dump. The `site_index` matches the dense
/// VM-local id assigned at executable build time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IcSiteSnapshot {
    /// Dense VM-local site id.
    pub site_index: u32,
    /// Opcode family the site belongs to.
    pub kind: IcSiteKind,
    /// Lifecycle state and entry list.
    pub state: IcSiteState,
}

/// One node in a hidden-class transition tree dump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShapeNodeSnapshot {
    /// VM-local shape id.
    pub shape_id: u64,
    /// Parent shape id, or `None` for the root.
    pub parent_shape_id: Option<u64>,
    /// Key added by this transition, or `None` for the root.
    pub transition_key: Option<String>,
    /// Number of string-keyed slots represented by this shape.
    pub property_count: u32,
}

/// Flat dump of the active hidden-class transition tree. Nodes
/// appear in deterministic order: root first, then transitions
/// sorted by `(parent_shape_id, transition_key)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShapeTransitionSnapshot {
    /// VM-local id of the root shape.
    pub root_shape_id: u64,
    /// Every shape reachable from the runtime transition table.
    pub nodes: Vec<ShapeNodeSnapshot>,
}

/// One transition event delivered to a
/// [`ShapeTransitionObserver`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShapeTransitionEvent {
    /// Parent shape id the transition starts from.
    pub from_shape_id: u64,
    /// Child shape id reached by the transition.
    pub to_shape_id: u64,
    /// Property key added by the transition.
    pub key: String,
    /// `true` when the transition table already had a cached entry
    /// for this `(parent, key)` pair — the observer fires every
    /// time the transition is taken, regardless of cache state.
    /// `false` means this dispatch allocated a fresh shape node.
    pub reused: bool,
}

/// Observer fired on every hidden-class transition. Useful for
/// shape-transition breakpoints and shape-thrash audits.
///
/// The observer runs inside the same mutator turn as the
/// allocating opcode; do not park, await, or call back into the
/// interpreter from inside `on_transition`.
pub trait ShapeTransitionObserver {
    /// Fired once per transition take. See
    /// [`ShapeTransitionEvent::reused`] for the cache-hit
    /// signal.
    fn on_transition(&mut self, event: &ShapeTransitionEvent);
}

/// One snapshot row inside a [`FrameSnapshot::registers`] list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisterSnapshot {
    /// Register index inside the frame's window.
    pub index: u16,
    /// Compact debug repr of the value (`int32:42`, `bool:true`,
    /// `undefined`, …).
    pub debug: String,
}

/// One frame's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameSnapshot {
    /// 1-based call depth (matches [`StepEvent::frame_depth`]).
    pub depth: usize,
    /// VM-local function id.
    pub function_id: u32,
    /// Source-declared function name.
    pub function_name: String,
    /// Byte-offset PC inside the function's encoded stream.
    pub byte_pc: u32,
    /// Total number of registers in this frame's window.
    pub register_count: usize,
    /// Registers in window order. Embedders that want every slot
    /// regardless of contents can pass `include_undefined = true`
    /// to the snapshot helper; the default drops `undefined` slots
    /// so the dump stays short for wide frames.
    pub registers: Vec<RegisterSnapshot>,
}

impl FrameSnapshot {
    /// Build a snapshot from the active dispatch frame visible
    /// through a [`StepEvent`]. `include_undefined` controls whether
    /// `undefined` register slots appear in the output.
    #[must_use]
    pub fn from_step_event(event: &StepEvent<'_>, include_undefined: bool) -> Self {
        let mut registers = Vec::with_capacity(event.register_window.len());
        for (idx, value) in event.register_window.iter().enumerate() {
            if !include_undefined && value.is_undefined() {
                continue;
            }
            let index = u16::try_from(idx).unwrap_or(u16::MAX);
            registers.push(RegisterSnapshot {
                index,
                debug: format_value_debug(*value),
            });
        }
        Self {
            depth: event.frame_depth,
            function_id: event.function_id,
            function_name: event.function_name.to_string(),
            byte_pc: event.byte_pc,
            register_count: event.register_window.len(),
            registers,
        }
    }
}

/// Build the [`IcSiteState`] DTO from one stored
/// [`crate::property_ic::PropertyIcEntry`] holding a
/// [`crate::cache_ir::CacheStub`].
#[must_use]
pub(crate) fn snapshot_load_state(
    entry: &crate::property_ic::PropertyIcEntry<crate::cache_ir::CacheStub>,
) -> IcSiteState {
    use crate::property_ic::PropertyIcEntry;
    match entry {
        PropertyIcEntry::Empty => IcSiteState::Empty,
        PropertyIcEntry::Megamorphic => IcSiteState::Megamorphic,
        PropertyIcEntry::Polymorphic { entries, misses } => {
            let mapped = entries
                .iter()
                .map(|ic| {
                    if let Some(hit) = ic.own_data_hit() {
                        IcEntrySnapshot {
                            variant: IcEntryVariant::OwnData,
                            receiver_shape_id: hit.shape_id.raw(),
                            key: None,
                            slot: Some(hit.slot),
                            to_shape_id: None,
                        }
                    } else if let Some((receiver_shape_id, hit)) = ic.direct_prototype_load() {
                        IcEntrySnapshot {
                            variant: IcEntryVariant::DirectPrototypeData,
                            receiver_shape_id: receiver_shape_id.raw(),
                            key: None,
                            slot: Some(hit.slot),
                            to_shape_id: None,
                        }
                    } else {
                        IcEntrySnapshot {
                            variant: IcEntryVariant::OwnData,
                            receiver_shape_id: 0,
                            key: None,
                            slot: None,
                            to_shape_id: None,
                        }
                    }
                })
                .collect();
            IcSiteState::Polymorphic {
                entries: mapped,
                misses: u32::from(*misses),
            }
        }
    }
}

/// Build the [`IcSiteState`] DTO from one
/// [`crate::property_ic::PropertyIcEntry`] holding a
/// [`crate::property_ic::StorePropertyIc`].
#[must_use]
pub(crate) fn snapshot_store_state(
    entry: &crate::property_ic::PropertyIcEntry<crate::property_ic::StorePropertyIc>,
) -> IcSiteState {
    use crate::property_ic::{PropertyIcEntry, StorePropertyIc};
    match entry {
        PropertyIcEntry::Empty => IcSiteState::Empty,
        PropertyIcEntry::Megamorphic => IcSiteState::Megamorphic,
        PropertyIcEntry::Polymorphic { entries, misses } => {
            let mapped = entries
                .iter()
                .map(|ic| match ic {
                    StorePropertyIc::ExistingOwnDataStore { hit } => IcEntrySnapshot {
                        variant: IcEntryVariant::OwnData,
                        receiver_shape_id: hit.shape_id.raw(),
                        key: None,
                        slot: Some(hit.slot),
                        to_shape_id: None,
                    },
                    StorePropertyIc::OwnAddTransition { transition } => IcEntrySnapshot {
                        variant: IcEntryVariant::OwnAddTransition,
                        receiver_shape_id: transition.from_shape_id.raw(),
                        key: None,
                        slot: None,
                        to_shape_id: Some(transition.to_shape_id.raw()),
                    },
                    StorePropertyIc::DirectPrototypeMissingTransition { transition } => {
                        IcEntrySnapshot {
                            variant: IcEntryVariant::DirectPrototypeMissingTransition,
                            receiver_shape_id: transition.from_shape_id.raw(),
                            key: None,
                            slot: None,
                            to_shape_id: Some(transition.to_shape_id.raw()),
                        }
                    }
                    StorePropertyIc::DirectPrototypeWritableDataTransition { transition } => {
                        IcEntrySnapshot {
                            variant: IcEntryVariant::DirectPrototypeWritableDataTransition,
                            receiver_shape_id: transition.from_shape_id.raw(),
                            key: None,
                            slot: None,
                            to_shape_id: Some(transition.to_shape_id.raw()),
                        }
                    }
                })
                .collect();
            IcSiteState::Polymorphic {
                entries: mapped,
                misses: u32::from(*misses),
            }
        }
    }
}

/// Build the [`IcSiteState`] DTO from one
/// [`crate::property_ic::PropertyIcEntry`] holding a
/// [`crate::cache_ir::CacheStub`].
#[must_use]
pub(crate) fn snapshot_has_state(
    entry: &crate::property_ic::PropertyIcEntry<crate::cache_ir::CacheStub>,
) -> IcSiteState {
    use crate::property_ic::PropertyIcEntry;
    match entry {
        PropertyIcEntry::Empty => IcSiteState::Empty,
        PropertyIcEntry::Megamorphic => IcSiteState::Megamorphic,
        PropertyIcEntry::Polymorphic { entries, misses } => {
            let mapped = entries
                .iter()
                .map(|ic| {
                    if let Some(hit) = ic.has_own_slot_hit() {
                        IcEntrySnapshot {
                            variant: IcEntryVariant::OwnData,
                            receiver_shape_id: hit.shape_id.raw(),
                            key: None,
                            slot: Some(hit.slot),
                            to_shape_id: None,
                        }
                    } else if let Some((receiver_shape_id, hit)) = ic.has_direct_prototype() {
                        IcEntrySnapshot {
                            variant: IcEntryVariant::DirectPrototypeData,
                            receiver_shape_id: receiver_shape_id.raw(),
                            key: None,
                            slot: Some(hit.slot),
                            to_shape_id: None,
                        }
                    } else {
                        IcEntrySnapshot {
                            variant: IcEntryVariant::OwnData,
                            receiver_shape_id: 0,
                            key: None,
                            slot: None,
                            to_shape_id: None,
                        }
                    }
                })
                .collect();
            IcSiteState::Polymorphic {
                entries: mapped,
                misses: u32::from(*misses),
            }
        }
    }
}

/// Build a [`ShapeTransitionSnapshot`] from the live shape runtime.
#[must_use]
pub(crate) fn build_shape_transition_snapshot(
    shape_runtime: &crate::object::ShapeRuntime,
    heap: &otter_gc::GcHeap,
) -> ShapeTransitionSnapshot {
    use crate::object::ShapeBody;
    use crate::string::to_utf16_vec;

    let root_handle = shape_runtime.root();
    let root_shape_id = heap.read_payload(root_handle, ShapeBody::id).raw();

    let mut nodes = Vec::new();
    nodes.push(ShapeNodeSnapshot {
        shape_id: root_shape_id,
        parent_shape_id: None,
        transition_key: None,
        property_count: 0,
    });

    let mut raw: Vec<(u64, u64, String, u32)> = Vec::new();
    for (parent_id, child) in shape_runtime.transitions_for_snapshot() {
        let (child_id, transition_key_handle, property_count, _own_offset) =
            heap.read_payload(child, |body| {
                (
                    body.id(),
                    body.transition_key(),
                    body.property_count(),
                    body.own_offset(),
                )
            });
        let key = if transition_key_handle.is_null() {
            String::new()
        } else {
            let units = to_utf16_vec(heap, transition_key_handle);
            String::from_utf16_lossy(&units)
        };
        raw.push((parent_id.raw(), child_id.raw(), key, property_count));
    }
    raw.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.2.cmp(&b.2)));
    for (parent_shape_id, shape_id, transition_key, property_count) in raw {
        nodes.push(ShapeNodeSnapshot {
            shape_id,
            parent_shape_id: Some(parent_shape_id),
            transition_key: Some(transition_key),
            property_count,
        });
    }

    ShapeTransitionSnapshot {
        root_shape_id,
        nodes,
    }
}

/// One bucket of the heap snapshot type-count summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeapTypeBucket {
    /// `Traceable::TYPE_TAG` value. Bytecode bodies define their
    /// own constants — see the `*_BODY_TYPE_TAG` table in the VM
    /// crate.
    pub type_tag: u8,
    /// Number of live objects with this tag.
    pub object_count: u32,
    /// Sum of `self_size` (header + payload) over those objects.
    pub bytes: u64,
}

/// Type-count summary of every live GC body. Stable across runs
/// for the same JS workload; one row per non-empty type tag,
/// sorted by descending `bytes`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeapSnapshotSummary {
    /// Total live object count across all type tags.
    pub object_count: u64,
    /// Total live bytes across all type tags (header + payload).
    pub total_bytes: u64,
    /// Per-tag totals. Empty tags are not represented.
    pub buckets: Vec<HeapTypeBucket>,
}

impl HeapSnapshotSummary {
    /// Build a summary from a raw [`otter_gc::HeapSnapshot`].
    #[must_use]
    pub fn from_snapshot(snapshot: &otter_gc::HeapSnapshot) -> Self {
        let totals = snapshot.group_by_type();
        let mut counts = [0u32; 256];
        let mut object_count: u64 = 0;
        for obj in &snapshot.objects {
            counts[obj.type_tag as usize] = counts[obj.type_tag as usize].saturating_add(1);
            object_count += 1;
        }
        let mut buckets: Vec<HeapTypeBucket> = (0..256u16)
            .filter_map(|tag| {
                let bytes = totals[tag as usize] as u64;
                let object_count = counts[tag as usize];
                if bytes == 0 && object_count == 0 {
                    return None;
                }
                Some(HeapTypeBucket {
                    type_tag: tag as u8,
                    object_count,
                    bytes,
                })
            })
            .collect();
        buckets.sort_by(|a, b| {
            b.bytes
                .cmp(&a.bytes)
                .then_with(|| a.type_tag.cmp(&b.type_tag))
        });
        let total_bytes = buckets.iter().map(|b| b.bytes).sum();
        Self {
            object_count,
            total_bytes,
            buckets,
        }
    }

    /// Render the summary as a deterministic text table — one line
    /// per bucket, columns: `type_tag`, `object_count`, `bytes`.
    /// Caller can pipe straight to stderr / a file from inspector
    /// commands.
    #[must_use]
    pub fn render_text(&self) -> String {
        use std::fmt::Write as _;
        let mut out = String::with_capacity(64 + self.buckets.len() * 32);
        let _ = writeln!(
            out,
            "; otter heap snapshot summary v1 — objects={} total_bytes={}",
            self.object_count, self.total_bytes,
        );
        let _ = writeln!(out, "  type_tag  object_count  bytes");
        for bucket in &self.buckets {
            let _ = writeln!(
                out,
                "  {:#04x}      {:>12}  {:>10}",
                bucket.type_tag, bucket.object_count, bucket.bytes,
            );
        }
        out
    }
}

/// Compact debug repr for a [`Value`]. Mirrors the kinds the
/// step trace surfaces in operand listings.
#[must_use]
pub fn format_value_debug(value: Value) -> String {
    if value.is_undefined() {
        "undefined".to_string()
    } else if value.is_null() {
        "null".to_string()
    } else if let Some(b) = value.as_boolean() {
        format!("bool:{b}")
    } else if let Some(n) = value.as_number() {
        format!("number:{}", n.as_f64())
    } else {
        // Fall back to the raw NaN-boxed bit pattern when the value
        // is heap-shaped — the dump should remain stable across
        // heap layouts.
        format!("bits:{:#018x}", value.to_bits())
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
        let operands = [
            Operand::Register(2),
            Operand::Register(0),
            Operand::Register(1),
        ];
        let registers: [Value; 0] = [];
        let event = StepEvent {
            frame_depth: 1,
            function_id: 0,
            function_name: "<main>",
            byte_pc: 12,
            op: Op::Add,
            operands: &operands,
            register_window: &registers,
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
            let registers: [Value; 0] = [];
            let event = StepEvent {
                frame_depth: 1,
                function_id: 0,
                function_name: "<main>",
                byte_pc: 0,
                op: Op::LoadInt32,
                operands: &operands,
                register_window: &registers,
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
        Op::FreshUpvalue,
        Op::Call,
        Op::TailCall,
        Op::IsEvalIntrinsic,
        Op::CallWithThis,
        Op::BindFunction,
        Op::LoadThis,
        Op::LoadNewTarget,
        Op::Throw,
        Op::EnterTry,
        Op::LeaveTry,
        Op::EndFinally,
        Op::NewError,
        Op::GeneratorStart,
        Op::GetIterator,
        Op::GetAsyncIterator,
        Op::IteratorNext,
        Op::IteratorClose,
        Op::IteratorCloseStart,
        Op::IteratorCloseEnd,
        Op::ArrayPush,
        Op::CallSpread,
        Op::New,
        Op::NewSpread,
        Op::SuperConstructSpread,
        Op::BindThisValue,
        Op::LoadSuperProperty,
        Op::LoadSuperElement,
        Op::SetSuperProperty,
        Op::SetSuperElement,
        Op::JumpViaFinally,
        Op::MakeClass,
        Op::MathLoad,
        Op::MathCall,
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
        Op::ImportNamespaceDeferred,
        Op::EvaluateModule,
        Op::MarkModuleEvaluated,
        Op::PromiseFulfilledOf,
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
        Op::StoreUpvalueChecked,
        Op::DeclareGlobalVar,
        Op::StarReexport,
        Op::ModuleNamespaceObject,
        Op::LoadImportBinding,
        Op::LoadDynamic,
        Op::StoreDynamic,
        Op::TypeofDynamic,
        Op::DeleteDynamic,
        Op::NewPrivateName,
        Op::DefineGlobalFunction,
        Op::DeclareGlobalLex,
        Op::StoreGlobalBinding,
        Op::InitGlobalLex,
        Op::ValidateGlobalDecl,
        Op::ToObject,
        Op::ToNumeric,
        Op::PrivateGet,
        Op::PrivateSet,
        Op::YieldDelegate,
        Op::DefineDataProperty,
        Op::SetFunctionName,
        Op::ClassCheck,
        Op::ToPropertyKey,
        Op::Increment,
        Op::PrivateBrandCheck,
        Op::LoadShadowedUpvalue,
        Op::GetTemplateObject,
    ];
}
