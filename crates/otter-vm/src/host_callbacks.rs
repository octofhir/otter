use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use crate::interpreter::RuntimeState;

type HostCallback = Box<dyn FnOnce(&mut RuntimeState) + Send + 'static>;

#[derive(Clone)]
pub struct HostCallbackSender {
    sender: Sender<HostCallback>,
    pending: Arc<AtomicUsize>,
}

impl HostCallbackSender {
    pub fn enqueue<F>(&self, callback: F) -> Result<(), String>
    where
        F: FnOnce(&mut RuntimeState) + Send + 'static,
    {
        self.pending.fetch_add(1, Ordering::SeqCst);
        if self.sender.send(Box::new(callback)).is_err() {
            self.pending.fetch_sub(1, Ordering::SeqCst);
            return Err("host callback queue is closed".into());
        }
        Ok(())
    }

    pub fn reserve(&self) -> HostCallbackReservation {
        self.pending.fetch_add(1, Ordering::SeqCst);
        HostCallbackReservation {
            sender: self.clone(),
            active: true,
        }
    }
}

pub struct HostCallbackReservation {
    sender: HostCallbackSender,
    active: bool,
}

impl HostCallbackReservation {
    pub fn enqueue<F>(mut self, callback: F) -> Result<(), String>
    where
        F: FnOnce(&mut RuntimeState) + Send + 'static,
    {
        self.active = false;
        if self.sender.sender.send(Box::new(callback)).is_err() {
            self.sender.pending.fetch_sub(1, Ordering::SeqCst);
            return Err("host callback queue is closed".into());
        }
        Ok(())
    }
}

impl Drop for HostCallbackReservation {
    fn drop(&mut self) {
        if self.active {
            self.sender.pending.fetch_sub(1, Ordering::SeqCst);
        }
    }
}

pub struct HostCallbackQueue {
    sender: HostCallbackSender,
    receiver: Receiver<HostCallback>,
}

impl HostCallbackQueue {
    pub fn new() -> Self {
        let (sender, receiver) = channel();
        let pending = Arc::new(AtomicUsize::new(0));
        Self {
            sender: HostCallbackSender { sender, pending },
            receiver,
        }
    }

    pub fn sender(&self) -> HostCallbackSender {
        self.sender.clone()
    }

    pub fn has_pending(&self) -> bool {
        self.sender.pending.load(Ordering::SeqCst) > 0
    }

    pub fn drain_ready(&mut self) -> Vec<HostCallback> {
        let mut callbacks = Vec::new();
        while let Ok(callback) = self.receiver.try_recv() {
            callbacks.push(callback);
        }
        callbacks
    }

    pub fn wait_and_drain(&mut self, timeout: Option<Duration>) -> Vec<HostCallback> {
        let first = match timeout {
            Some(timeout) => self.receiver.recv_timeout(timeout).ok(),
            None => self.receiver.recv().ok(),
        };

        let Some(first) = first else {
            return Vec::new();
        };

        let mut callbacks = vec![first];
        callbacks.extend(self.drain_ready());
        callbacks
    }

    pub fn complete_one(&self) {
        self.sender.pending.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Default for HostCallbackQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RuntimeState;

    #[test]
    fn sender_enqueues_and_marks_pending() {
        let mut queue = HostCallbackQueue::new();
        let sender = queue.sender();
        assert!(!queue.has_pending());

        sender
            .enqueue(|_runtime| {})
            .expect("enqueue should succeed");
        assert!(queue.has_pending());
        assert_eq!(queue.drain_ready().len(), 1);
    }

    #[test]
    fn invoke_clears_pending() {
        let mut queue = HostCallbackQueue::new();
        let sender = queue.sender();
        sender
            .enqueue(|runtime| {
                runtime.install_global_value("__ok", crate::RegisterValue::from_bool(true));
            })
            .expect("enqueue should succeed");

        let callback = queue
            .drain_ready()
            .into_iter()
            .next()
            .expect("callback should be ready");
        let mut runtime = RuntimeState::new();
        queue.complete_one();
        callback(&mut runtime);

        assert!(!queue.has_pending());
        let global = runtime.intrinsics().global_object();
        let property = runtime.intern_property_name("__ok");
        assert_eq!(
            runtime
                .own_property_value(global, property)
                .expect("property should exist"),
            crate::RegisterValue::from_bool(true)
        );
    }

    #[test]
    fn reservation_marks_pending_before_callback_is_ready() {
        let queue = HostCallbackQueue::new();
        let reservation = queue.sender().reserve();
        assert!(queue.has_pending());
        drop(reservation);
        assert!(!queue.has_pending());
    }
}
