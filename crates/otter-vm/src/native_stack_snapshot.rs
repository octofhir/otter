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
//! - The walk reads scalar metadata only, neither collecting nor invoking JS.
//!
//! # See also
//! - [`crate::stack_snapshot`] — source resolution shared with interpreter views.
//! - [`crate::Interpreter::jit_push_native_frame`] — publication lifetime.

use crate::native_abi::NativeFrameFlags;
use crate::{ActivationStack, ExecutionContext, Interpreter, StackFrameSnapshot};

impl Interpreter {
    pub(crate) fn snapshot_active_frames(
        &self,
        context: &ExecutionContext,
        stack: &ActivationStack,
        limit: usize,
    ) -> Vec<StackFrameSnapshot> {
        let mut sites = Vec::with_capacity(stack.len() + self.jit_native_activation_top);
        let mut owners = vec![None; stack.len()];
        let mut cursor = 0;
        for activation in &self.jit_native_activations[..self.jit_native_activation_top] {
            // SAFETY: publication keeps this header live through the complete
            // metadata-only walk. No managed window is borrowed or dereferenced.
            let native = unsafe { &*activation.frame };
            let transferred =
                self.jit_materialized_generated_calls
                    .iter()
                    .find_map(|&(address, index)| {
                        (address == activation.frame as usize).then_some(index)
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
                    (native.header.function_id, native.header.pc, true)
                };
                if let Some(position) = owners[index] {
                    sites[position] = site;
                } else {
                    owners[index] = Some(sites.len());
                    sites.push(site);
                }
                cursor = cursor.max(index + 1);
            } else {
                sites.push((native.header.function_id, native.header.pc, true));
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
}
