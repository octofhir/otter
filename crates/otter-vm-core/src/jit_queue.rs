#![cfg(feature = "jit")]

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

use otter_vm_bytecode::{Function, Module};

#[derive(Debug, Clone)]
pub(crate) struct JitCompileRequest {
    pub(crate) module_id: u64,
    pub(crate) module_source_url: String,
    pub(crate) function_index: u32,
    pub(crate) function_name: Option<String>,
    pub(crate) call_count: u32,
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

/// Lock the queue state, recovering from poisoned mutex (test resilience).
fn lock_queue() -> std::sync::MutexGuard<'static, JitQueueState> {
    queue_state()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) fn enqueue_hot_function(
    module: &Arc<Module>,
    function_index: u32,
    function: &Function,
) -> bool {
    let key = (module.module_id, function_index);
    let mut state = lock_queue();

    if !state.enqueued.insert(key) {
        return false;
    }

    state.pending.push_back(JitCompileRequest {
        module_id: key.0,
        module_source_url: module.source_url.clone(),
        function_index,
        function_name: function.name.clone(),
        call_count: function.get_call_count(),
        function: function.clone(),
    });
    true
}

pub(crate) fn pop_next_request() -> Option<JitCompileRequest> {
    let mut state = lock_queue();

    let next = state.pending.pop_front()?;
    state
        .enqueued
        .remove(&(next.module_id, next.function_index));
    Some(next)
}

pub(crate) fn pending_count() -> usize {
    let state = lock_queue();
    state.pending.len()
}

#[cfg(test)]
pub(crate) fn clear_for_tests() {
    let mut state = lock_queue();
    state.pending.clear();
    state.enqueued.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use otter_vm_bytecode::{Function, Instruction, Module, Register};

    fn make_test_module() -> Arc<Module> {
        let mut builder = Module::builder("jit-queue-test.js");
        builder.add_function(
            Function::builder()
                .name("hot_candidate")
                .instruction(Instruction::LoadInt32 {
                    dst: Register(0),
                    value: 1,
                })
                .instruction(Instruction::Return { src: Register(0) })
                .build(),
        );
        Arc::new(builder.build())
    }

    #[test]
    fn enqueue_deduplicates_until_popped() {
        clear_for_tests();
        let module = make_test_module();
        let func = module.function(0).expect("function 0 must exist");
        func.mark_hot();

        assert!(enqueue_hot_function(&module, 0, func));
        assert!(!enqueue_hot_function(&module, 0, func));
        assert_eq!(pending_count(), 1);

        let req = pop_next_request().expect("expected queued request");
        assert_eq!(req.module_source_url, "jit-queue-test.js");
        assert_eq!(req.function_index, 0);
        assert_eq!(req.function_name.as_deref(), Some("hot_candidate"));
        assert_eq!(req.function.instructions.len(), 2);

        assert_eq!(pending_count(), 0);
        assert!(enqueue_hot_function(&module, 0, func));
    }
}
