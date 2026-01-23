//! Microtask queue

use parking_lot::Mutex;
use std::collections::VecDeque;

/// Microtask callback type
pub type Microtask = Box<dyn FnOnce() + Send>;

/// Queue of microtasks
pub struct MicrotaskQueue {
    queue: Mutex<VecDeque<Microtask>>,
}

impl MicrotaskQueue {
    /// Create new empty queue
    pub fn new() -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
        }
    }

    /// Add a microtask to the queue
    pub fn enqueue<F>(&self, task: F)
    where
        F: FnOnce() + Send + 'static,
    {
        self.queue.lock().push_back(Box::new(task));
    }

    /// Take the next microtask
    pub fn dequeue(&self) -> Option<Microtask> {
        self.queue.lock().pop_front()
    }

    /// Check if queue is empty
    pub fn is_empty(&self) -> bool {
        self.queue.lock().is_empty()
    }
}

impl Default for MicrotaskQueue {
    fn default() -> Self {
        Self::new()
    }
}
