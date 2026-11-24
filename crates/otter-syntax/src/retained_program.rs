//! An unchanged OXC parse over one retained source owner.
//!
//! # Contents
//! - `RetainedProgram` keeps AST storage and exact source alive together.
//!
//! # Invariants
//! The AST can borrow only its own source and arena. Clients see a borrow of
//! that AST and never pass it to the VM. A parse requiring compatibility
//! recovery belongs to the normal parser entry and is not retained here.
//!
//! # See also
//! - `super::with_program_goal` for the ordinary borrowed syntax pipeline.

use otter_resource::SharedSource;
use oxc_ast::ast::Program;
use self_cell::self_cell;

use crate::{SourceGoal, SourceKind, parser_arena::ParserArena};

struct ParseStorage {
    source: SharedSource,
    arena: ParserArena,
}

self_cell!(
    struct Parsed {
        owner: ParseStorage,

        #[covariant]
        dependent: Program,
    }
);

/// An owned syntax arena and its unchanged parsed program.
pub struct RetainedProgram(Parsed);

impl RetainedProgram {
    /// Parse the admitted text without changing its source coordinates.
    /// Returns `None` when the ordinary parser must diagnose or recover it.
    pub fn parse(source: SharedSource, kind: SourceKind, goal: SourceGoal) -> Option<Self> {
        let owner = ParseStorage {
            source,
            arena: ParserArena::acquire(),
        };
        Parsed::try_new(owner, |owner| {
            let parsed = crate::parse_once(&owner.arena, &owner.source, kind, goal);
            if parsed.diagnostics.is_empty() {
                Ok(parsed.program)
            } else {
                Err(())
            }
        })
        .ok()
        .map(Self)
    }

    /// The borrowed AST, valid while this source owner remains alive.
    #[must_use]
    pub fn program(&self) -> &Program<'_> {
        self.0.borrow_dependent()
    }

    /// Bytes reserved by the retained AST arena.
    #[must_use]
    pub fn arena_capacity(&self) -> usize {
        self.0.borrow_owner().arena.capacity()
    }

    /// True only for this exact source allocation and range.
    #[must_use]
    pub fn owns(&self, text: &str) -> bool {
        std::ptr::eq(self.0.borrow_owner().source.as_ref(), text)
    }
}
