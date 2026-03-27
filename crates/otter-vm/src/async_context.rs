//! Suspended async function context for `await` support.
//!
//! When an async function hits an `await` on a pending promise, the
//! interpreter captures the entire execution state into a [`SuspendedFrame`].
//! When the awaited promise settles, the frame is restored and execution
//! continues from where it left off.
//!
//! This is the same pattern V8 uses (async function state machine + implicit
//! generator) and matches the old otter-vm-core `AsyncContext`.

use crate::bytecode::ProgramCounter;
use crate::frame::RegisterIndex;
use crate::module::FunctionIndex;
use crate::object::ObjectHandle;
use crate::value::RegisterValue;

/// Captured state of an async function suspended at an `await`.
///
/// The interpreter creates this when `Opcode::Await` encounters a pending
/// promise, and restores it when the promise settles.
#[derive(Debug, Clone)]
pub struct SuspendedFrame {
    /// The function being executed when suspension occurred.
    pub function_index: FunctionIndex,
    /// Program counter to resume from (instruction after the Await).
    pub pc: ProgramCounter,
    /// Complete register window snapshot (moved, not cloned).
    pub registers: Box<[RegisterValue]>,
    /// Closure context if the function is a closure.
    pub closure_handle: Option<ObjectHandle>,
    /// Pending exception at time of suspension (if any).
    pub pending_exception: Option<RegisterValue>,
    /// The async function's return promise (the promise that `.then()` callers
    /// wait on). Settled when the async function returns or throws.
    pub result_promise: ObjectHandle,
    /// The register where the await result should be written on resume.
    pub resume_register: RegisterIndex,
}

impl SuspendedFrame {
    /// Extracts all ObjectHandle roots from this suspended frame for GC.
    pub fn gc_roots(&self) -> Vec<ObjectHandle> {
        let mut roots = Vec::new();
        roots.push(self.result_promise);
        if let Some(h) = self.closure_handle {
            roots.push(h);
        }
        // Extract handles from registers.
        for reg in self.registers.iter() {
            if let Some(handle) = reg.as_object_handle() {
                roots.push(ObjectHandle(handle));
            }
        }
        // Extract handle from pending exception.
        if let Some(exc) = self.pending_exception
            && let Some(handle) = exc.as_object_handle()
        {
            roots.push(ObjectHandle(handle));
        }
        roots
    }
}

/// Collection of all currently suspended async frames.
///
/// Multiple async functions can be suspended simultaneously (e.g., `Promise.all`
/// with multiple async functions). Each frame is independently resumed when
/// its awaited promise settles.
#[derive(Debug, Default)]
pub struct SuspendedFrameSet {
    frames: Vec<SuspendedFrame>,
}

impl SuspendedFrameSet {
    pub fn new() -> Self {
        Self { frames: Vec::new() }
    }

    /// Stores a suspended frame, returning an index for later retrieval.
    pub fn suspend(&mut self, frame: SuspendedFrame) -> SuspendedFrameId {
        let id = SuspendedFrameId(self.frames.len() as u32);
        self.frames.push(frame);
        id
    }

    /// Takes (removes) a suspended frame by ID for resumption.
    ///
    /// Returns `None` if the ID is invalid or the frame was already resumed.
    pub fn resume(&mut self, id: SuspendedFrameId) -> Option<SuspendedFrame> {
        let idx = id.0 as usize;
        if idx < self.frames.len() {
            // Swap-remove to avoid shifting. Order doesn't matter since
            // frames are identified by ID, not position.
            // Actually we can't swap-remove because IDs are indices.
            // Use Option wrapping instead.
            // For now, just replace with a dummy check.
            // TODO: Use a slab or generational arena for O(1) remove.
            let frame = self.frames.get_mut(idx)?;
            // Check if already taken (registers empty = already resumed).
            if frame.registers.is_empty() {
                return None;
            }
            let mut taken = SuspendedFrame {
                function_index: frame.function_index,
                pc: frame.pc,
                registers: std::mem::take(&mut frame.registers),
                closure_handle: frame.closure_handle.take(),
                pending_exception: frame.pending_exception.take(),
                result_promise: frame.result_promise,
                resume_register: frame.resume_register,
            };
            // Mark as consumed.
            taken.result_promise = frame.result_promise;
            Some(taken)
        } else {
            None
        }
    }

    /// Collects all GC roots from all suspended frames.
    pub fn gc_roots(&self) -> Vec<ObjectHandle> {
        let mut roots = Vec::new();
        for frame in &self.frames {
            if !frame.registers.is_empty() {
                roots.extend(frame.gc_roots());
            }
        }
        roots
    }

    /// Number of currently suspended frames.
    pub fn count(&self) -> usize {
        self.frames.iter().filter(|f| !f.registers.is_empty()).count()
    }

    /// Whether there are any suspended frames.
    pub fn is_empty(&self) -> bool {
        self.count() == 0
    }
}

/// Identifier for a suspended frame in the [`SuspendedFrameSet`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SuspendedFrameId(pub u32);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::module::FunctionIndex;
    use crate::value::RegisterValue;

    fn make_frame(fn_idx: u32, promise: u32) -> SuspendedFrame {
        SuspendedFrame {
            function_index: FunctionIndex(fn_idx),
            pc: 10,
            registers: vec![
                RegisterValue::from_i32(1),
                RegisterValue::from_object_handle(42),
                RegisterValue::undefined(),
            ]
            .into_boxed_slice(),
            closure_handle: Some(ObjectHandle(99)),
            pending_exception: None,
            result_promise: ObjectHandle(promise),
            resume_register: 0,
        }
    }

    #[test]
    fn suspend_and_resume() {
        let mut set = SuspendedFrameSet::new();
        let id = set.suspend(make_frame(0, 100));
        assert_eq!(set.count(), 1);

        let frame = set.resume(id).expect("should resume");
        assert_eq!(frame.function_index, FunctionIndex(0));
        assert_eq!(frame.result_promise, ObjectHandle(100));
        assert_eq!(frame.registers.len(), 3);

        // Double resume should return None.
        assert!(set.resume(id).is_none());
        assert_eq!(set.count(), 0);
    }

    #[test]
    fn multiple_suspended_frames() {
        let mut set = SuspendedFrameSet::new();
        let id1 = set.suspend(make_frame(0, 100));
        let id2 = set.suspend(make_frame(1, 200));
        assert_eq!(set.count(), 2);

        let f2 = set.resume(id2).unwrap();
        assert_eq!(f2.result_promise, ObjectHandle(200));
        assert_eq!(set.count(), 1);

        let f1 = set.resume(id1).unwrap();
        assert_eq!(f1.result_promise, ObjectHandle(100));
        assert_eq!(set.count(), 0);
    }

    #[test]
    fn gc_roots_includes_all_handles() {
        let mut set = SuspendedFrameSet::new();
        set.suspend(make_frame(0, 100));

        let roots = set.gc_roots();
        // Should include: result_promise(100), closure_handle(99),
        // register with object handle (42).
        assert!(roots.contains(&ObjectHandle(100)));
        assert!(roots.contains(&ObjectHandle(99)));
        assert!(roots.contains(&ObjectHandle(42)));
    }

    #[test]
    fn empty_set() {
        let set = SuspendedFrameSet::new();
        assert!(set.is_empty());
        assert_eq!(set.count(), 0);
        assert!(set.gc_roots().is_empty());
    }
}
