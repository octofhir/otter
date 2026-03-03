use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

use otter_vm_bytecode::{Constant, Function, Module};
use otter_vm_jit::translator::can_translate_function_with_helpers;

/// Maximum instruction count for a function to be eligible for inlining.
const INLINE_BUDGET: usize = 32;

#[derive(Debug, Clone)]
pub(crate) struct JitCompileRequest {
    pub(crate) module_id: u64,
    pub(crate) function_index: u32,
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
    });

    true
}

pub(crate) fn pop_next_request() -> Option<JitCompileRequest> {
    let mut state = lock_queue();
    while let Some(pending) = state.pending.pop_front() {
        let Some(function) = pending.module.function(pending.function_index) else {
            state
                .enqueued
                .remove(&(pending.module_id, pending.function_index));
            continue;
        };

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
                || !func.upvalues.is_empty()
            {
                continue;
            }
            // Check that the callee doesn't use LoadThis (this-binding differs when inlined)
            let has_load_this = instrs
                .iter()
                .any(|inst| matches!(inst, otter_vm_bytecode::Instruction::LoadThis { .. }));
            if has_load_this {
                continue;
            }
            module_functions.push((idx, func.clone()));
        }

        return Some(JitCompileRequest {
            module_id: pending.module_id,
            function_index: pending.function_index,
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
}
