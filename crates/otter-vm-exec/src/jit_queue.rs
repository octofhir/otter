use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

use otter_vm_bytecode::function::InlineCacheState;
use otter_vm_bytecode::function::UpvalueCapture;
use otter_vm_bytecode::{Constant, Function, Module};
use otter_vm_jit::opt::compiler::can_translate_function_with_helpers;

/// Maximum instruction count for a function to be eligible for inlining.
const INLINE_BUDGET: usize = 32;
/// Maximum number of dequeue deferrals for functions with completely empty feedback.
const MAX_EMPTY_FEEDBACK_DEFERRALS: u8 = 16;

#[derive(Debug, Clone)]
pub(crate) struct JitCompileRequest {
    pub(crate) module_id: u64,
    pub(crate) function_index: u32,
    pub(crate) module_source_url: String,
    pub(crate) function_name: Option<String>,
    pub(crate) constants: Vec<Constant>,
    pub(crate) function: Function,
    /// Small eligible functions from the same module available for inlining.
    /// Each entry is (function_index, Function).
    pub(crate) module_functions: Vec<(u32, Function)>,
}

#[derive(Debug, Clone)]
struct PendingJitCompileRequest {
    module: Arc<Module>,
    module_id: u64,
    function_index: u32,
    empty_feedback_deferrals: u8,
}

#[derive(Default)]
struct JitQueueState {
    pending: VecDeque<PendingJitCompileRequest>,
    enqueued: HashSet<(u64, u32)>,
}

static JIT_QUEUE: OnceLock<Mutex<JitQueueState>> = OnceLock::new();

fn queue_state() -> &'static Mutex<JitQueueState> {
    JIT_QUEUE.get_or_init(|| Mutex::new(JitQueueState::default()))
}

fn lock_queue() -> std::sync::MutexGuard<'static, JitQueueState> {
    queue_state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[inline]
fn has_feedback_observations(function: &Function) -> bool {
    function.feedback_vector.read().iter().any(|metadata| {
        metadata.hit_count > 0
            || metadata.type_observations != Default::default()
            || !matches!(metadata.ic_state, InlineCacheState::Uninitialized)
    })
}

#[inline]
fn inlineable_local_upvalues(function: &Function) -> bool {
    function
        .upvalues
        .iter()
        .all(|capture| matches!(capture, UpvalueCapture::Local(_)))
}

#[inline]
fn has_inline_unsupported_upvalue_ops(function: &Function) -> bool {
    function.instructions.read().iter().any(|inst| {
        matches!(
            inst,
            otter_vm_bytecode::Instruction::SetUpvalue { .. }
                | otter_vm_bytecode::Instruction::CloseUpvalue { .. }
                | otter_vm_bytecode::Instruction::LoadThis { .. }
        )
    })
}

/// Enqueue a hot function for JIT compilation.
pub fn enqueue_hot_function(
    module: &Arc<Module>,
    function_index: u32,
    function: &Function,
) -> bool {
    let constants: Vec<Constant> = module.constants.iter().cloned().collect();

    if !can_translate_function_with_helpers(function, &constants) {
        return false;
    }

    let key = (module.module_id, function_index);
    let mut state = lock_queue();

    if !state.enqueued.insert(key) {
        return false;
    }

    state.pending.push_back(PendingJitCompileRequest {
        module: Arc::clone(module),
        module_id: key.0,
        function_index,
        empty_feedback_deferrals: 0,
    });

    true
}

pub(crate) fn pop_next_request() -> Option<JitCompileRequest> {
    let mut state = lock_queue();
    let mut remaining = state.pending.len();
    while remaining > 0 {
        remaining -= 1;
        let Some(mut pending) = state.pending.pop_front() else {
            break;
        };
        let Some(function) = pending.module.function(pending.function_index) else {
            state
                .enqueued
                .remove(&(pending.module_id, pending.function_index));
            continue;
        };

        // Defer compilation when the function has a feedback vector but nothing
        // has been observed yet. This avoids freezing an "all-default" snapshot
        // too early. Cap deferrals to prevent starvation for low-feedback code.
        let feedback_len = function.feedback_vector.read().len();
        if feedback_len > 0
            && !has_feedback_observations(function)
            && pending.empty_feedback_deferrals < MAX_EMPTY_FEEDBACK_DEFERRALS
        {
            pending.empty_feedback_deferrals = pending.empty_feedback_deferrals.saturating_add(1);
            state.pending.push_back(pending);
            continue;
        }

        let constants: Vec<Constant> = pending.module.constants.iter().cloned().collect();
        if !can_translate_function_with_helpers(function, &constants) {
            state
                .enqueued
                .remove(&(pending.module_id, pending.function_index));
            continue;
        }

        // Snapshot inline candidates at dequeue-time so feedback is as fresh as possible.
        let mut module_functions = Vec::new();
        for (idx, func) in pending.module.functions.iter().enumerate() {
            let idx = idx as u32;
            if idx == pending.function_index {
                continue; // Don't self-inline
            }
            let instrs = func.instructions.read();
            if instrs.len() > INLINE_BUDGET {
                continue;
            }
            if func.flags.is_async
                || func.flags.is_generator
                || func.flags.has_rest
                || func.flags.uses_eval
                || func.flags.uses_arguments
                || !inlineable_local_upvalues(func)
                || has_inline_unsupported_upvalue_ops(func)
            {
                continue;
            }
            module_functions.push((idx, func.clone()));
        }

        return Some(JitCompileRequest {
            module_id: pending.module_id,
            function_index: pending.function_index,
            module_source_url: pending.module.source_url.clone(),
            function_name: function.name.clone(),
            constants,
            function: function.clone(),
            module_functions,
        });
    }

    None
}

pub(crate) fn mark_request_finished(module_id: u64, function_index: u32) {
    let mut state = lock_queue();
    state.enqueued.remove(&(module_id, function_index));
}

/// Number of pending compile requests.
pub fn pending_count() -> usize {
    let state = lock_queue();
    state.pending.len()
}

/// Clear queue and dedup state.
///
/// Used by tests that run multiple JIT scenarios in one process.
pub fn clear_for_tests() {
    let mut state = lock_queue();
    state.pending.clear();
    state.enqueued.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_vm_bytecode::function::UpvalueCapture;
    use otter_vm_bytecode::{FunctionIndex, LocalIndex};
    use otter_vm_bytecode::{Instruction, Module, Register};

    fn build_test_module() -> Arc<Module> {
        let function = Function::builder()
            .name("jit_queue_test")
            .register_count(1)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 7,
            })
            .instruction(Instruction::Return { src: Register(0) })
            .build();

        let mut builder = Module::builder("jit-queue-test.js");
        let index = builder.add_function(function);
        Arc::new(builder.entry_point(index).build())
    }

    fn build_yield_module() -> Arc<Module> {
        let function = Function::builder()
            .name("jit_queue_yield_test")
            .register_count(2)
            .instruction(Instruction::LoadInt32 {
                dst: Register(1),
                value: 1,
            })
            .instruction(Instruction::Yield {
                dst: Register(0),
                src: Register(1),
            })
            .instruction(Instruction::Return { src: Register(0) })
            .build();

        let mut builder = Module::builder("jit-queue-yield-test.js");
        let index = builder.add_function(function);
        Arc::new(builder.entry_point(index).build())
    }

    fn build_async_flagged_module() -> Arc<Module> {
        let function = Function::builder()
            .name("jit_queue_async_flag_test")
            .register_count(1)
            .is_async(true)
            .instruction(Instruction::ReturnUndefined)
            .build();

        let mut builder = Module::builder("jit-queue-async-flag-test.js");
        let index = builder.add_function(function);
        Arc::new(builder.entry_point(index).build())
    }

    fn build_generator_flagged_module() -> Arc<Module> {
        let function = Function::builder()
            .name("jit_queue_generator_flag_test")
            .register_count(1)
            .is_generator(true)
            .instruction(Instruction::ReturnUndefined)
            .build();

        let mut builder = Module::builder("jit-queue-generator-flag-test.js");
        let index = builder.add_function(function);
        Arc::new(builder.entry_point(index).build())
    }

    #[test]
    fn keeps_key_reserved_until_request_is_marked_finished() {
        let _guard = crate::test_lock();
        clear_for_tests();

        let module = build_test_module();
        let function = module
            .function(0)
            .expect("test module should expose entry function");

        assert!(enqueue_hot_function(&module, 0, function));
        assert!(pop_next_request().is_some());
        assert!(!enqueue_hot_function(&module, 0, function));

        mark_request_finished(module.module_id, 0);
        assert!(enqueue_hot_function(&module, 0, function));

        clear_for_tests();
    }

    #[test]
    fn enqueues_functions_with_yield_opcode() {
        let _guard = crate::test_lock();
        clear_for_tests();

        let module = build_yield_module();
        let function = module
            .function(0)
            .expect("test module should expose entry function");

        assert!(
            enqueue_hot_function(&module, 0, function),
            "functions with Yield opcode should be eligible for JIT (bail-on-yield)"
        );
        assert_eq!(pending_count(), 1);

        clear_for_tests();
    }

    #[test]
    fn does_not_enqueue_async_functions() {
        let _guard = crate::test_lock();
        clear_for_tests();

        let async_module = build_async_flagged_module();
        let async_function = async_module
            .function(0)
            .expect("test module should expose entry function");
        assert!(
            !enqueue_hot_function(&async_module, 0, async_function),
            "async functions must remain non-eligible for baseline JIT"
        );

        assert_eq!(pending_count(), 0);
        clear_for_tests();
    }

    #[test]
    fn enqueues_generator_functions() {
        let _guard = crate::test_lock();
        clear_for_tests();

        let generator_module = build_generator_flagged_module();
        let generator_function = generator_module
            .function(0)
            .expect("test module should expose entry function");
        assert!(
            enqueue_hot_function(&generator_module, 0, generator_function),
            "generator functions should be eligible for JIT (bail-on-yield)"
        );

        assert_eq!(pending_count(), 1);
        clear_for_tests();
    }

    #[test]
    fn snapshots_inline_candidates_with_local_upvalues_only() {
        let _guard = crate::test_lock();
        clear_for_tests();

        let outer = Function::builder()
            .name("outer")
            .register_count(8)
            .local_count(4)
            .instruction(Instruction::Closure {
                dst: Register(0),
                func: FunctionIndex(1),
            })
            .instruction(Instruction::SetLocal {
                idx: LocalIndex(2),
                src: Register(0),
            })
            .instruction(Instruction::Closure {
                dst: Register(1),
                func: FunctionIndex(2),
            })
            .instruction(Instruction::SetLocal {
                idx: LocalIndex(3),
                src: Register(1),
            })
            .instruction(Instruction::Closure {
                dst: Register(2),
                func: FunctionIndex(3),
            })
            .instruction(Instruction::SetLocal {
                idx: LocalIndex(0),
                src: Register(2),
            })
            .instruction(Instruction::ReturnUndefined)
            .build();

        let add = Function::builder()
            .name("add")
            .register_count(3)
            .local_count(2)
            .instruction(Instruction::GetLocal {
                dst: Register(0),
                idx: LocalIndex(0),
            })
            .instruction(Instruction::GetLocal {
                dst: Register(1),
                idx: LocalIndex(1),
            })
            .instruction(Instruction::Add {
                dst: Register(2),
                lhs: Register(0),
                rhs: Register(1),
                feedback_index: 0,
            })
            .instruction(Instruction::Return { src: Register(2) })
            .feedback_vector_size(1)
            .build();

        let call_chain = Function::builder()
            .name("call_chain")
            .register_count(6)
            .local_count(2)
            .upvalues(vec![
                UpvalueCapture::Local(LocalIndex(2)),
                UpvalueCapture::Local(LocalIndex(3)),
            ])
            .instruction(Instruction::GetUpvalue {
                dst: Register(0),
                idx: LocalIndex(0),
            })
            .instruction(Instruction::GetLocal {
                dst: Register(1),
                idx: LocalIndex(0),
            })
            .instruction(Instruction::GetLocal {
                dst: Register(2),
                idx: LocalIndex(1),
            })
            .instruction(Instruction::Call {
                dst: Register(3),
                func: Register(0),
                argc: 2,
                ic_index: 0,
            })
            .instruction(Instruction::Return { src: Register(3) })
            .build();

        let transitive_capture = Function::builder()
            .name("transitive_capture")
            .register_count(1)
            .local_count(0)
            .upvalues(vec![UpvalueCapture::Upvalue(LocalIndex(0))])
            .instruction(Instruction::ReturnUndefined)
            .build();

        let mut builder = Module::builder("jit-queue-inline-upvalues.js");
        let outer_idx = builder.add_function(outer);
        builder.add_function(add);
        builder.add_function(call_chain);
        builder.add_function(transitive_capture);
        let module = Arc::new(builder.entry_point(outer_idx).build());

        let outer = module
            .function(outer_idx)
            .expect("test module should expose entry function");
        assert!(enqueue_hot_function(&module, outer_idx, outer));

        let request = pop_next_request().expect("request should be available");
        let module_function_indices: Vec<u32> = request
            .module_functions
            .iter()
            .map(|(idx, _)| *idx)
            .collect();
        assert!(
            module_function_indices.contains(&2),
            "small local-upvalue closure should stay inline-eligible"
        );
        assert!(
            !module_function_indices.contains(&3),
            "transitively captured closure must stay out of inline candidates"
        );

        mark_request_finished(module.module_id, outer_idx);
        clear_for_tests();
    }

    #[test]
    fn snapshots_feedback_at_dequeue_time() {
        let _guard = crate::test_lock();
        clear_for_tests();

        let function = Function::builder()
            .name("jit_queue_feedback_snapshot")
            .register_count(1)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 7,
            })
            .instruction(Instruction::Return { src: Register(0) })
            .feedback_vector_size(1)
            .build();

        let mut builder = Module::builder("jit-queue-feedback-test.js");
        let index = builder.add_function(function);
        let module = Arc::new(builder.entry_point(index).build());

        let function = module
            .function(0)
            .expect("test module should expose entry function");
        assert!(enqueue_hot_function(&module, 0, function));

        // Mutate feedback after enqueue; dequeue snapshot must include this update.
        function.feedback_vector.write()[0]
            .type_observations
            .observe_int32();

        let request = pop_next_request().expect("request should be available");
        assert!(
            request.function.feedback_vector.read()[0]
                .type_observations
                .seen_int32
        );

        mark_request_finished(module.module_id, 0);
        clear_for_tests();
    }

    #[test]
    fn defers_empty_feedback_until_observed() {
        let _guard = crate::test_lock();
        clear_for_tests();

        let function = Function::builder()
            .name("jit_queue_feedback_deferral")
            .register_count(1)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 7,
            })
            .instruction(Instruction::Return { src: Register(0) })
            .feedback_vector_size(1)
            .build();

        let mut builder = Module::builder("jit-queue-feedback-deferral.js");
        let index = builder.add_function(function);
        let module = Arc::new(builder.entry_point(index).build());

        let function = module
            .function(0)
            .expect("test module should expose entry function");
        assert!(enqueue_hot_function(&module, 0, function));

        // First dequeue should defer because feedback is still all-default.
        assert!(pop_next_request().is_none());
        assert_eq!(pending_count(), 1);

        // Populate feedback and verify next dequeue snapshots the update.
        function.feedback_vector.write()[0]
            .type_observations
            .observe_int32();
        let request = pop_next_request().expect("request should be available after feedback");
        assert!(
            request.function.feedback_vector.read()[0]
                .type_observations
                .seen_int32
        );

        mark_request_finished(module.module_id, 0);
        clear_for_tests();
    }

    #[test]
    fn empty_feedback_deferral_has_starvation_cap() {
        let _guard = crate::test_lock();
        clear_for_tests();

        let function = Function::builder()
            .name("jit_queue_feedback_deferral_cap")
            .register_count(1)
            .instruction(Instruction::LoadInt32 {
                dst: Register(0),
                value: 7,
            })
            .instruction(Instruction::Return { src: Register(0) })
            .feedback_vector_size(1)
            .build();

        let mut builder = Module::builder("jit-queue-feedback-deferral-cap.js");
        let index = builder.add_function(function);
        let module = Arc::new(builder.entry_point(index).build());

        let function = module
            .function(0)
            .expect("test module should expose entry function");
        assert!(enqueue_hot_function(&module, 0, function));

        for _ in 0..MAX_EMPTY_FEEDBACK_DEFERRALS {
            assert!(
                pop_next_request().is_none(),
                "request should still be deferred while under cap"
            );
        }

        assert!(
            pop_next_request().is_some(),
            "request should compile after starvation cap even without feedback"
        );

        mark_request_finished(module.module_id, 0);
        clear_for_tests();
    }
}
