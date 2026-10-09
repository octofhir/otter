//! The published JavaScript activations as the language sees them: each
//! physical frame, preceded by the activations it runs inlined (V8's
//! `FrameInspector` over an optimized frame's frame state).
//!
//! # Contents
//! - [`LogicalActivation`] — one activation's function, callee and, for an
//!   inlined one, its actual arguments.
//! - [`Interpreter::logical_activations`] — the published chain, innermost
//!   first.
//!
//! # Invariants
//! - A compiled frame suspended at a generated call describes the
//!   activations it runs inlined in that call's safepoint record: their
//!   callee and actual arguments are homes of the frame or constants, read
//!   without allocation.
//! - An inlined activation the record does not describe (a frame stopped in
//!   a runtime helper rather than a call) is not listed.
//! - Host frames carry no JavaScript activation and are not listed.
//!
//! # See also
//! - [`crate::native_stack_snapshot`] for the same walk over source sites.
//! - `otter_jit::graph::metadata` for the records' inline descriptions.

use crate::deopt::{DeoptLocation, DeoptSlot};
use crate::native_abi::{Frame, NativeFrameKind};
use crate::{Interpreter, Value};

/// One JavaScript activation of the published chain.
pub(crate) struct LogicalActivation {
    /// Function the activation runs.
    pub(crate) function_id: u32,
    /// Exact callable (SELF) of the activation.
    pub(crate) callee: Value,
    /// The physical frame that is, or runs inlined, this activation.
    pub(crate) frame: *const Frame,
    /// An inlined activation's actual arguments, when its frame recorded
    /// them; `None` for a physical activation, whose frame holds them.
    pub(crate) inlined_arguments: Option<Vec<Value>>,
    /// Whether the activation runs inlined in `frame`.
    pub(crate) inlined: bool,
}

impl Interpreter {
    /// Every JavaScript activation of the published chain, innermost first.
    pub(crate) fn logical_activations(&self) -> Vec<LogicalActivation> {
        let mut activations = Vec::new();
        for (address, child) in self.jit_native_frames_with_children() {
            // SAFETY: every published record stays live while it is linked.
            let frame = unsafe { &*address };
            if frame.header.kind == NativeFrameKind::Host {
                continue;
            }
            let anchor = self.jit_child_return_pc(frame, child);
            if let Ok(Some(record)) = self.jit_anchored_safepoint(frame, anchor) {
                // Inline descendants are recorded outermost first.
                for inline in record.inline_frames.iter().rev() {
                    let Some(callee) = inline
                        .entry
                        .and_then(|entry| read_suspended(frame, entry.closure))
                    else {
                        continue;
                    };
                    let inlined_arguments = inline.arguments.as_ref().and_then(|slots| {
                        slots
                            .iter()
                            .map(|&slot| read_suspended(frame, slot))
                            .collect::<Option<Vec<_>>>()
                    });
                    activations.push(LogicalActivation {
                        function_id: inline.function_id,
                        callee,
                        frame: address,
                        inlined_arguments,
                        inlined: true,
                    });
                }
            }
            activations.push(LogicalActivation {
                function_id: frame.header.function_id,
                callee: frame.self_value,
                frame: address,
                inlined_arguments: None,
                inlined: false,
            });
        }
        activations
    }
}

/// The value `slot` names in suspended `frame`: a home under its machine
/// roots or a constant.
fn read_suspended(frame: &Frame, slot: DeoptSlot) -> Option<Value> {
    if matches!(slot.location, DeoptLocation::StackSlot(_)) && frame.machine_roots == 0 {
        return None;
    }
    if !matches!(
        slot.location,
        DeoptLocation::StackSlot(_) | DeoptLocation::Literal(_)
    ) {
        return None;
    }
    slot.reconstitute(|location| match location {
        // SAFETY: the record names a home of this suspended frame's spill
        // area, which stays live and holds the value while the frame is
        // published at this call.
        DeoptLocation::StackSlot(offset) => unsafe {
            (frame.machine_roots as *const u8)
                .offset(offset as isize)
                .cast::<u64>()
                .read()
        },
        DeoptLocation::Literal(bits) => bits,
        DeoptLocation::Register(_) | DeoptLocation::VirtualObject(_) => {
            unreachable!("filtered above")
        }
    })
}
