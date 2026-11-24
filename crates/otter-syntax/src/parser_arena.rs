//! Reuse OXC's parsing arena on the thread that consumed the AST.
//!
//! # Contents
//! - `ParserArena` owns the current parse and returns its storage on drop.
//!
//! # Invariants
//! The arena leaves the thread-local slot while a parse is running. A
//! nested parse therefore acquires a different arena. Parser callbacks
//! cannot keep AST borrows after the parse, and oversized arenas are freed.
//!
//! # See also
//! - `super::with_program_goal` bounds the lifetime of the parsed program.

use std::cell::Cell;
use std::ops::Deref;

use oxc_allocator::Allocator;

const MAX_IDLE_CAPACITY: usize = 2 * 1024 * 1024;

thread_local! {
    static IDLE: Cell<Option<Allocator>> = const { Cell::new(None) };
}

pub(super) struct ParserArena(Option<Allocator>);

impl ParserArena {
    pub(super) fn acquire() -> Self {
        let arena = IDLE.try_with(Cell::take).ok().flatten().unwrap_or_default();
        Self(Some(arena))
    }
}

impl Deref for ParserArena {
    type Target = Allocator;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref().expect("a running parse owns an arena")
    }
}

impl Drop for ParserArena {
    fn drop(&mut self) {
        let mut arena = self.0.take().expect("a running parse owns an arena");
        arena.reset();
        if arena.capacity() <= MAX_IDLE_CAPACITY {
            let _ = IDLE.try_with(|idle| idle.set(Some(arena)));
        }
    }
}
