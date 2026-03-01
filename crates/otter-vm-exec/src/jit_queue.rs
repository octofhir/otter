use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

use otter_vm_bytecode::{Constant, Function, Module};
use otter_vm_jit::translator::can_translate_function_with_helpers;

#[derive(Debug, Clone)]
pub(crate) struct JitCompileRequest {
    pub(crate) module_id: u64,
    pub(crate) function_index: u32,
    pub(crate) constants: Vec<Constant>,
    pub(crate) function: Function,
}

#[derive(Default)]
struct JitQueueState {
    pending: VecDeque<JitCompileRequest>,
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

    state.pending.push_back(JitCompileRequest {
        module_id: key.0,
        function_index,
        constants,
        function: function.clone(),
    });

    true
}

pub(crate) fn pop_next_request() -> Option<JitCompileRequest> {
    let mut state = lock_queue();
    state.pending.pop_front()
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
    fn does_not_enqueue_functions_with_yield_bailout_stub_opcode() {
        let _guard = crate::test_lock();
        clear_for_tests();

        let module = build_yield_module();
        let function = module
            .function(0)
            .expect("test module should expose entry function");

        assert!(
            !enqueue_hot_function(&module, 0, function),
            "yield/await bailout stubs should not pass JIT eligibility"
        );
        assert_eq!(pending_count(), 0);

        clear_for_tests();
    }

    #[test]
    fn does_not_enqueue_async_or_generator_functions() {
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

        let generator_module = build_generator_flagged_module();
        let generator_function = generator_module
            .function(0)
            .expect("test module should expose entry function");
        assert!(
            !enqueue_hot_function(&generator_module, 0, generator_function),
            "generator functions must remain non-eligible for baseline JIT"
        );

        assert_eq!(pending_count(), 0);
        clear_for_tests();
    }
}
