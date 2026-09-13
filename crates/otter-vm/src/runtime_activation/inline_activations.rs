//! Scoped native publication of inlined activations during committed reentry.
//!
//! # Contents
//! - Boxed instances of the existing frame recipes become canonical native frames.
//! - A lexical publication scope exposes the innermost activation to VM operations.
//!
//! # Invariants
//! - The already-published caller is retained, never copied or replayed.
//! - Register recipes are boxed before entry. Host frame/spine allocation cannot
//!   collect; every native frame is published before the semantic operation.
//! - Generated hits need no activation records. Only committed cold reentry uses
//!   this scope, and the operation must normalize JS exceptions before it exits.
//! - Every publication is removed on normal return, failure, or Rust unwinding.
//! - Captured handles and this/SELF remain precise moving roots. Direct eval,
//!   fresh capture cells, and incoming-arguments bodies require richer entry
//!   recipes and are rejected before effects.
//!
//! # See also
//! - `crate::native_stack_snapshot` observes the same native activation chain.
//! - `crate::deopt::DeoptFrame` is the single frame/entry recipe owner.

use std::ptr::NonNull;

use crate::{
    Interpreter, Value, VmError,
    deopt::DeoptFrame,
    native_abi::{NativeFrame, NativeFrameKind, VmFrameHeader},
};

use super::{RuntimeCall, RuntimeFrameIdentity};

struct InlinePublication {
    vm: NonNull<Interpreter>,
    base: usize,
    recipes: *mut DeoptFrame<Value>,
    natives: *const NativeFrame,
    count: usize,
}

impl Drop for InlinePublication {
    fn drop(&mut self) {
        // SAFETY: the scope never outlives its exclusive RuntimeCall. All frame
        // and window owners were declared before the scope and remain live.
        let vm = unsafe { self.vm.as_mut() };
        for index in 0..self.count {
            // SAFETY: both fully initialized slices remain stationary until
            // this guard drops, including while the semantic operation unwinds.
            let native = unsafe { &*self.natives.add(index) };
            let recipe = unsafe { &mut *self.recipes.add(index) };
            let entry = recipe.entry.as_mut().expect("validated inline entry");
            entry.this = native.this_value();
            entry.closure = native.self_value();
        }
        while vm.jit_native_activation_top > self.base {
            vm.jit_pop_native_activation();
        }
    }
}

impl RuntimeCall<'_> {
    /// Publish only the inlined descendants of this already-live activation.
    /// `frames` is outermost-first and every frame has an exact entry recipe.
    /// Its source offset is a byte PC, as in the shared deopt schema.
    ///
    /// This is private compiled-engine plumbing. The caller supplies current
    /// boxed recipes and keeps its allocator roots published across the scope.
    /// No instruction is interpreted and no return destination is written.
    pub fn with_inline_activations<T>(
        &mut self,
        frames: &mut [DeoptFrame<Value>],
        operation: impl FnOnce(&mut RuntimeCall<'_>) -> T,
    ) -> Result<T, VmError> {
        if frames.is_empty() {
            return Ok(operation(self));
        }
        // SAFETY: RuntimeCall binds these live service owners. Preparation
        // below only allocates Rust containers; it never collects or reenters.
        let vm = unsafe { &mut *self.vm.as_ptr() };
        let context = unsafe { self.context.as_ref() };
        let stack = unsafe { self.stack.as_ref() };
        let base = vm.jit_native_activation_top;
        if base == 0 || vm.jit_native_activations[base - 1].frame != self.frame.as_ptr() {
            return Err(VmError::InvalidOperand);
        }
        if frames.len() > vm.jit_generated_activation_limit().saturating_sub(base)
            || frames.len() as u64 + u64::from(vm.logical_call_depth(stack))
                > u64::from(vm.max_stack_depth)
        {
            return Err(VmError::StackOverflow {
                limit: vm.max_stack_depth,
            });
        }
        let mut natives = Vec::with_capacity(frames.len());
        let mut spines = Vec::with_capacity(frames.len());
        // SAFETY: RuntimeCall retains this validated, already-published caller.
        let mut parent_registers =
            usize::from(unsafe { self.frame.as_ref() }.header.register_count);
        for recipe in frames.iter_mut() {
            let entry = recipe.entry.as_ref().ok_or(VmError::InvalidOperand)?;
            let function = context
                .exec_function(recipe.function_id)
                .ok_or(VmError::InvalidOperand)?;
            if recipe.slots.len() != usize::from(function.register_count)
                || usize::from(entry.return_register) >= parent_registers
                || function.own_upvalue_count != 0
                || function.needs_arguments
                || function.contains_direct_eval
            {
                return Err(VmError::InvalidOperand);
            }
            let pc = (0..function.code.len())
                .find(|&index| function.instruction_byte_pc(index) == Some(recipe.byte_pc))
                .and_then(|index| u32::try_from(index).ok())
                .ok_or(VmError::InvalidOperand)?;
            let upvalues = if let Some(closure) = entry.closure.as_closure(&vm.gc_heap) {
                let header = closure.call_header(&vm.gc_heap);
                if closure.function_id() != recipe.function_id
                    || header.requires_runtime_setup()
                    || !header.eval_env.is_null()
                {
                    return Err(VmError::InvalidOperand);
                }
                closure.upvalues_snapshot(&vm.gc_heap).into_boxed_slice()
            } else if entry.closure.as_function_id() == Some(recipe.function_id) {
                Box::default()
            } else {
                return Err(VmError::InvalidOperand);
            };
            if upvalues.len() != usize::from(function.inherited_upvalue_count) {
                return Err(VmError::InvalidOperand);
            }
            let mut native = NativeFrame::new(
                VmFrameHeader {
                    pc,
                    kind: NativeFrameKind::Optimizing,
                    ..VmFrameHeader::interpreter(recipe.function_id, function.register_count)
                },
                recipe.slots.as_mut_ptr() as u64,
                entry.closure,
                entry.this,
            );
            native.set_stack_registers();
            native.set_upvalue_window(upvalues.as_ptr() as u64, upvalues.len() as u32);
            spines.push(upvalues);
            natives.push(native);
            parent_registers = recipe.slots.len();
        }
        let publication = InlinePublication {
            vm: self.vm,
            base,
            recipes: frames.as_mut_ptr(),
            natives: natives.as_ptr(),
            count: frames.len(),
        };
        for native in &mut natives {
            // SAFETY: vectors are fully built; frames, boxed slots and spines
            // stay stationary until the lexical publication is dropped.
            unsafe { vm.jit_push_native_frame(native) }?;
        }
        let mut inner = RuntimeCall {
            vm: self.vm,
            stack: self.stack,
            context: self.context,
            frame: NonNull::from(natives.last_mut().expect("nonempty inline chain")),
            identity: RuntimeFrameIdentity::StackOwned,
            _exclusive: std::marker::PhantomData,
        };
        let result = operation(&mut inner);
        // The collector rewrites registers in place; the guard returns current
        // this/SELF to the recipe owner before unpublishing the native frames.
        drop(publication);
        Ok(result)
    }
}

#[cfg(test)]
#[path = "inline_activations_tests.rs"]
mod tests;
