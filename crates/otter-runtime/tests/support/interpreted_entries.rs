//! Interpreted entries of loop-free functions in a fixed native-call workload.
//!
//! # Contents
//! A step tracer counts actual bytecode entries by source name. The caller
//! supplies the number of JavaScript invocations from its fixed workload.
//!
//! # Invariants
//! Each subject has a unique name, executes logical PC zero once per entry,
//! and contains no loop header for OSR. A subject must enter the interpreter
//! at least once so a misspelled name cannot pass the native execution proof.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use otter_runtime::{
    Runtime,
    inspect::{StepEvent, StepTracer},
};

#[derive(Clone, Default)]
pub struct InterpretedEntries(Arc<Mutex<BTreeMap<String, u64>>>);

impl InterpretedEntries {
    pub fn observe(runtime: &mut Runtime) -> Self {
        let entries = Self::default();
        runtime.set_tracer(Some(Box::new(entries.clone())));
        entries
    }

    pub fn native_calls(&self, name: &str, calls: u64) -> u64 {
        let entries = self.0.lock().unwrap();
        let count = entries
            .get(name)
            .unwrap_or_else(|| panic!("missing interpreted entry for {name} in {entries:?}"));
        calls.checked_sub(*count).expect("fixed invocation count")
    }
}

impl StepTracer for InterpretedEntries {
    fn on_step(&mut self, event: &StepEvent<'_>) {
        if event.byte_pc == 0 {
            *self
                .0
                .lock()
                .unwrap()
                .entry(event.function_name.to_owned())
                .or_default() += 1;
        }
    }
}
