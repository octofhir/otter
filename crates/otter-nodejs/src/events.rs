//! EventEmitter module - Node.js-compatible event system
//!
//! EventEmitter is implemented primarily in JavaScript.
//! This module provides any native helpers if needed.

/// No native ops needed - EventEmitter is pure JS
pub fn events_ops() -> Vec<otter_vm_runtime::extension::Op> {
    vec![]
}
