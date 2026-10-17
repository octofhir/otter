//! Checked storage for lexical bindings shared by switch clauses.
//!
//! # Contents
//! - Cross-clause reference proof over the existing AST capture index.
//! - Names whose initialization can be bypassed by selector entry.
//!
//! # Invariants
//! A clause-private binding may remain a register: execution reaches its
//! initializer before every later read in that clause. A binding referenced
//! from another clause uses the canonical hole-bearing context slot. Nested or
//! shadowed references can only make this proof more conservative.
//!
//! # See also
//! - `capture` owns the sorted AST reference positions.
//! - `hoist` chooses storage at lexical-scope birth.
//! - `scope` owns checked loads and stores.

use crate::HashSet;

use oxc_ast::ast::SwitchCase;

use crate::{capture::CaptureFacts, hoist::hoist_lexical_names};

pub(crate) fn checked_lexical_names(
    capture: &CaptureFacts,
    cases: &[SwitchCase<'_>],
) -> HashSet<String> {
    let mut checked = HashSet::default();
    for (declaration_case, case) in cases.iter().enumerate() {
        let mut names = Vec::new();
        hoist_lexical_names(&case.consequent, &mut names);
        for (name, _) in names {
            if cases.iter().enumerate().any(|(index, other)| {
                index != declaration_case
                    && capture.references_in_range(&name, (other.span.start, other.span.end))
            }) {
                checked.insert(name);
            }
        }
    }
    checked
}
