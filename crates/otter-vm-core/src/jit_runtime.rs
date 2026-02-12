#![cfg(feature = "jit")]

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

use otter_vm_jit::JitCompiler;

use crate::jit_queue;

#[derive(Default)]
struct JitRuntimeState {
    compiler: Option<JitCompiler>,
    compiled_code_ptrs: HashMap<(usize, u32), usize>,
    compile_errors: u64,
}

static JIT_RUNTIME_STATE: OnceLock<Mutex<JitRuntimeState>> = OnceLock::new();

fn runtime_state() -> &'static Mutex<JitRuntimeState> {
    JIT_RUNTIME_STATE.get_or_init(|| Mutex::new(JitRuntimeState::default()))
}

/// Compile one pending JIT request, if present.
pub(crate) fn compile_one_pending_request() {
    let Some(request) = jit_queue::pop_next_request() else {
        return;
    };

    let mut state = runtime_state()
        .lock()
        .expect("jit runtime mutex should not be poisoned");

    if state.compiler.is_none() {
        match JitCompiler::new() {
            Ok(compiler) => state.compiler = Some(compiler),
            Err(_) => {
                state.compile_errors = state.compile_errors.saturating_add(1);
                return;
            }
        }
    }

    let compiler = state
        .compiler
        .as_mut()
        .expect("jit compiler should be initialized");
    match compiler.compile_function(&request.function) {
        Ok(artifact) => {
            state.compiled_code_ptrs.insert(
                (request.module_ptr, request.function_index),
                artifact.code_ptr as usize,
            );
        }
        Err(_) => {
            state.compile_errors = state.compile_errors.saturating_add(1);
        }
    }
}

#[cfg(test)]
pub(crate) fn clear_for_tests() {
    let mut state = runtime_state()
        .lock()
        .expect("jit runtime mutex should not be poisoned");
    state.compiler = None;
    state.compiled_code_ptrs.clear();
    state.compile_errors = 0;
}

#[cfg(test)]
pub(crate) fn compiled_count() -> usize {
    let state = runtime_state()
        .lock()
        .expect("jit runtime mutex should not be poisoned");
    state.compiled_code_ptrs.len()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use otter_vm_bytecode::{Function, Instruction, Module, Register};

    use super::*;

    #[test]
    fn compile_one_pending_request_consumes_queue_item() {
        crate::jit_queue::clear_for_tests();
        clear_for_tests();

        let mut builder = Module::builder("jit-runtime-test.js");
        builder.add_function(
            Function::builder()
                .name("f")
                .instruction(Instruction::LoadInt32 {
                    dst: Register(0),
                    value: 1,
                })
                .instruction(Instruction::Return { src: Register(0) })
                .build(),
        );
        let module = Arc::new(builder.build());
        let function = module.function(0).expect("function 0 should exist");
        function.mark_hot();

        assert!(crate::jit_queue::enqueue_hot_function(&module, 0, function));
        assert_eq!(crate::jit_queue::pending_count(), 1);

        compile_one_pending_request();

        assert_eq!(crate::jit_queue::pending_count(), 0);
        assert_eq!(compiled_count(), 1);
    }
}
