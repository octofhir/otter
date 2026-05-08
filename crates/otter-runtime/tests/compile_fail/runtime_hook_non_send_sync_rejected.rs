//! Runtime hooks must not capture isolate-local or thread-local state.

use std::cell::Cell;
use std::rc::Rc;

use otter_runtime::{Diagnostic, RuntimeDiagnosticHook};

struct LocalDiagnosticHook {
    _state: Rc<Cell<u32>>,
}

impl RuntimeDiagnosticHook for LocalDiagnosticHook {
    fn emit_diagnostic(&self, _diagnostic: &Diagnostic) {}
}

fn main() {
    let _hook = LocalDiagnosticHook {
        _state: Rc::new(Cell::new(0)),
    };
}
