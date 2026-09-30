//! Stack diagnostics across generated calls and interpreter reentry.
//!
//! # Contents
//! - Merging published native frames with their materialized activation owners.
//! - Resolving source locations through the shared borrowed snapshot resolver.
//!
//! # Invariants
//! - Native PCs already identify the call or throw instruction; only suspended
//!   interpreter PCs need the usual one-instruction adjustment.
//! - Cold materialization supplies exact identity, never a function-name guess.
//! - An OSR frame updates its materialized owner instead of duplicating it.
//! - A frame making a generated call reports its call site's recorded PC,
//!   followed by the inline parents that call site's recipe describes; the
//!   parents have no frames.
//! - The walk reads scalar metadata only, neither collecting nor invoking JS.
//!
//! # See also
//! - [`crate::stack_snapshot`] — source resolution shared with interpreter views.
//! - [`crate::Interpreter::jit_native_frames`] — the published frame chain.

use crate::native_abi::NativeFrameFlags;
use crate::{ActivationStack, ExecutionContext, Interpreter, StackFrameSnapshot};

impl Interpreter {
    pub(crate) fn snapshot_active_frames(
        &self,
        context: &ExecutionContext,
        stack: &ActivationStack,
        limit: usize,
    ) -> Vec<StackFrameSnapshot> {
        let mut natives: Vec<_> = self.jit_native_frames().collect();
        natives.reverse();
        let mut sites = Vec::with_capacity(stack.len() + natives.len());
        let mut owners = vec![None; stack.len()];
        let mut cursor = 0;
        let native_count = natives.len();
        for (position, address) in natives.into_iter().enumerate() {
            // SAFETY: publication keeps this header live through the complete
            // metadata-only walk. No managed window is borrowed or dereferenced.
            let native = unsafe { &*address };
            // A frame with an inner native frame is making a generated call:
            // its call site names the call's PC and any inline parents.
            let call = (position + 1 < native_count)
                .then(|| self.generated_call_record(native))
                .flatten();
            let native_pc = call
                .map(|record| record.call_pc)
                .filter(|&pc| pc != crate::native_abi::NO_CALL_PC)
                .unwrap_or(native.header.pc);
            let transferred =
                self.jit_materialized_generated_calls
                    .iter()
                    .find_map(|&(materialized, index)| {
                        (materialized == address as usize).then_some(index)
                    });
            let owner = transferred.or_else(|| {
                (!native
                    .header
                    .flags
                    .contains(NativeFrameFlags::STACK_REGISTERS))
                .then(|| {
                    stack.iter().position(|frame| {
                        frame.function_id == native.header.function_id
                            && frame.registers.as_ptr() as u64 == native.register_base
                    })
                })
                .flatten()
            });
            if let Some(index) = owner.filter(|&index| index < stack.len()) {
                while cursor < index {
                    owners[cursor] = Some(sites.len());
                    let frame = &stack[cursor];
                    sites.push((frame.function_id, frame.pc, false));
                    cursor += 1;
                }
                let site = if transferred.is_some() {
                    let frame = &stack[index];
                    (frame.function_id, frame.pc, false)
                } else {
                    (native.header.function_id, native_pc, true)
                };
                if let Some(position) = owners[index] {
                    sites[position] = site;
                } else {
                    owners[index] = Some(sites.len());
                    sites.push(site);
                }
                cursor = cursor.max(index + 1);
            } else {
                sites.push((native.header.function_id, native_pc, true));
            }
            if let Some(record) = call.filter(|record| record.inline_frames_virtual) {
                push_virtual_inline_sites(context, record, &mut sites);
            }
        }
        for frame in stack.iter().skip(cursor) {
            sites.push((frame.function_id, frame.pc, false));
        }
        let mut result = Vec::with_capacity(sites.len().min(limit));
        for (depth, (function, pc, exact)) in sites.into_iter().rev().take(limit).enumerate() {
            let pc = if exact || depth == 0 {
                pc
            } else {
                pc.saturating_sub(1)
            };
            crate::stack_snapshot::visit_frame_snapshot(context, function, pc as usize, |frame| {
                result.push(StackFrameSnapshot {
                    function_id: frame.function_id,
                    function_name: frame.function_name.to_owned(),
                    module: frame.module.to_owned(),
                    span: frame.span,
                });
                true
            });
        }
        result
    }

    fn generated_call_record(
        &self,
        native: &crate::native_abi::NativeFrame,
    ) -> Option<&crate::native_abi::SafepointRecord> {
        if native.call_site == crate::native_abi::NO_SAFEPOINT {
            return None;
        }
        self.jit_code_registry
            .safepoint_record(u64::from(native.code_object_id), native.call_site)
    }
}

fn push_virtual_inline_sites(
    context: &ExecutionContext,
    record: &crate::native_abi::SafepointRecord,
    sites: &mut Vec<(u32, u32, bool)>,
) {
    for frame in &record.inline_frames {
        let Some(function) = context.exec_function(frame.function_id) else {
            continue;
        };
        if let Some(pc) = (0..function.code.len())
            .find(|&index| function.instruction_byte_pc(index) == Some(frame.byte_pc))
            .and_then(|index| u32::try_from(index).ok())
        {
            sites.push((frame.function_id, pc, true));
        }
    }
}
