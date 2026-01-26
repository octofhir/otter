use std::collections::VecDeque;

use crate::value::Value;

/// Guard for iterative destruction of deep object graphs.
///
/// Recursive Drop implementations in Rust can cause stack overflows when
/// cleaning up deeply nested structures (like linked lists or deep object chains).
/// This guard processes destruction iteratively using a work queue.
pub struct DropGuard {
    queue: VecDeque<Value>,
}

impl DropGuard {
    /// Create a new drop guard
    pub fn new() -> Self {
        Self {
            queue: VecDeque::new(),
        }
    }

    /// Add a value to be destroyed
    pub fn push(&mut self, value: Value) {
        self.queue.push_back(value);
    }

    /// Run the destruction process until the queue is empty
    pub fn run(mut self) {
        while let Some(value) = self.queue.pop_front() {
            self.process_value(value);
        }
    }

    /// Process a single value
    fn process_value(&mut self, value: Value) {
        // We only care about objects and closures since they contain references
        if let Some(obj) = value.as_object() {
            // Use the object's method to extract all children (properties, elements, prototype)
            // and clear the object's storage to break cycles.
            let children = obj.clear_and_extract_values();
            for child in children {
                // Only push objects/functions which might need recursion breaking
                if child.is_object()
                    || child.is_function()
                    || child.is_promise()
                    || child.is_proxy()
                {
                    self.queue.push_back(child);
                }
            }
        } else if let Some(promise) = value.as_promise() {
            // Handle promises iteratively
            let children = promise.clear_and_extract_values();
            for child in children {
                if child.is_object()
                    || child.is_function()
                    || child.is_promise()
                    || child.is_proxy()
                {
                    self.queue.push_back(child);
                }
            }
        } else if let Some(closure) = value.as_function() {
            // Handle closure upvalues (LexicalEnvironment)
            for cell in &closure.upvalues {
                let val = cell.get();
                // If it's not undefined, clear it and queue the old value
                if !val.is_undefined() {
                    cell.set(Value::undefined());
                    if val.is_object() || val.is_function() || val.is_promise() || val.is_proxy() {
                        self.queue.push_back(val);
                    }
                }
            }
        }
    }
}

impl Default for DropGuard {
    fn default() -> Self {
        Self::new()
    }
}
