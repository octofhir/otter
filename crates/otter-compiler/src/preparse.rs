//! Preparse facts: what a lazily compiled function's body compile knows
//! about the functions inside it without parsing their bodies.
//!
//! The unit that parses a function whole analyzes every scope in it. When
//! the function becomes a lazy record, the facts of the functions inside it
//! go with the record ([`otter_bytecode::Preparse`]): the names each one
//! reaches outside itself, the names it assigns there and whether a direct
//! eval in it reaches out. The body compile
//! then blanks those functions' statements before parsing, so a function
//! nested `d` deep is parsed whole twice at most instead of `d + 1` times,
//! and the capture analysis takes the recorded facts where it would have
//! walked the bodies. A skipped function always compiles lazily, carrying
//! the facts of the functions inside it in turn.
//!
//! # Contents
//! - [`collect`] — the preparse tree of a function about to become a lazy
//!   record.
//! - [`SkipTable`] / [`SkippedFacts`] — the skipped functions of one body
//!   compile, by span, for the capture analysis and the compiler.
//! - [`blank`] — the body text with the skipped statements blanked.
//!
//! # Invariants
//! - Only plain `function` declarations and expressions are skipped:
//!   arrows see their enclosing function's `this`, `arguments`, `super` and
//!   `new.target`, and methods carry a home object, so their bodies stay.
//!   Functions the immediate-call heuristic compiles eagerly stay too.
//! - Blanking replaces bytes with spaces, so every offset in the blanked
//!   text is the offset in the original: spans, positions and
//!   [[SourceText]] ranges need no translation.
//! - A body compile with skipped functions lowers to the bytecode it lowers
//!   to without them. The one licence: a name a skipped function binds
//!   for itself no longer counts as a mention of the same spelling around
//!   it, which only spares a context slot or a TDZ check nothing needed.
//!
//! # See also
//! - `lazy` for the body compile.
//! - `capture` for the analysis the facts stand in for.

use oxc_ast::ast::FunctionBody;
use rustc_hash::FxHashMap;

use crate::capture::{CaptureFacts, FunctionNode};
use otter_bytecode::{Preparse, SkippedFunction};

/// Shortest body worth skipping, in bytes of statement text: below it the
/// record's facts cost about what parsing the body again does.
const MIN_SKIPPED_BODY: u32 = 256;

/// The skipped functions of one body compile, keyed by the start of their
/// span in the text the compile parses.
#[derive(Debug)]
pub(crate) struct SkipTable {
    preparse: Preparse,
    by_start: FxHashMap<u32, usize>,
}

/// The recorded facts of one skipped function.
#[derive(Clone, Copy)]
pub(crate) struct SkippedFacts<'p> {
    names: &'p [String],
    function: &'p SkippedFunction,
}

impl<'p> SkippedFacts<'p> {
    fn spell(&self, ids: &'p [u32]) -> impl Iterator<Item = &'p str> + 'p {
        let names = self.names;
        ids.iter().map(move |&id| names[id as usize].as_str())
    }

    /// Names it references and does not bind.
    pub(crate) fn escape(&self) -> impl Iterator<Item = &'p str> + 'p {
        self.spell(&self.function.escape)
    }

    /// Names it assigns and does not bind.
    pub(crate) fn escape_writes(&self) -> impl Iterator<Item = &'p str> + 'p {
        self.spell(&self.function.escape_writes)
    }

    /// A direct eval inside it reaches the scopes around it.
    pub(crate) fn escape_eval(&self) -> bool {
        self.function.escape_eval
    }

    /// A direct eval appears anywhere inside it.
    pub(crate) fn contains_eval(&self) -> bool {
        self.function.contains_eval
    }
}

impl SkipTable {
    /// The outermost skipped functions of `preparse`, with every offset of
    /// the tree moved by `delta` into the parsed text's, and the ranges to
    /// blank in that text.
    pub(crate) fn new(preparse: &Preparse, delta: i64) -> (Self, Vec<(u32, u32)>) {
        let mut preparse = preparse.clone();
        shift_all(&mut preparse.skipped, delta);
        let mut by_start = FxHashMap::default();
        let mut blanks = Vec::with_capacity(preparse.skipped.len());
        for (index, function) in preparse.skipped.iter().enumerate() {
            by_start.insert(function.span.0, index);
            blanks.push(function.blank);
        }
        (Self { preparse, by_start }, blanks)
    }

    /// The facts of the skipped function whose span starts at `start`.
    pub(crate) fn get(&self, start: u32) -> Option<SkippedFacts<'_>> {
        self.by_start.get(&start).map(|&index| SkippedFacts {
            names: &self.preparse.names,
            function: &self.preparse.skipped[index],
        })
    }

    /// Whether the function whose span starts at `start` was skipped.
    pub(crate) fn contains(&self, start: u32) -> bool {
        self.by_start.contains_key(&start)
    }
}

fn shift(offset: u32, delta: i64) -> u32 {
    u32::try_from(i64::from(offset) + delta).expect("preparse offset inside the parsed text")
}

/// Move every offset of a preparse forest by `delta`.
pub(crate) fn shift_all(functions: &mut [SkippedFunction], delta: i64) {
    for function in functions {
        function.span = (shift(function.span.0, delta), shift(function.span.1, delta));
        function.blank = (
            shift(function.blank.0, delta),
            shift(function.blank.1, delta),
        );
        shift_all(&mut function.children, delta);
    }
}

/// `text` with every range of `blanks` (offsets into `text`) overwritten by
/// spaces. Each range starts at a statement and ends at a closing brace,
/// both character boundaries.
pub(crate) fn blank(text: &mut String, blanks: &[(u32, u32)]) {
    let mut spaces = String::new();
    for &(start, end) in blanks {
        let (start, end) = (start as usize, end as usize);
        spaces.clear();
        spaces.extend(std::iter::repeat_n(' ', end - start));
        text.replace_range(start..end, &spaces);
    }
}

/// The preparse tree of a function this body compile skipped: the facts of
/// the skipped functions inside it, as recorded.
pub(crate) fn of_skipped(skips: &SkipTable, start: u32) -> Option<Box<Preparse>> {
    let facts = skips.get(start)?;
    let capture = CaptureFacts::default();
    let mut collector = Collector::new(&capture, None);
    let skipped: Vec<SkippedFunction> = facts
        .function
        .children
        .iter()
        .map(|function| {
            collector.adopt(SkippedFacts {
                names: facts.names,
                function,
            })
        })
        .collect();
    (!skipped.is_empty()).then(|| {
        Box::new(Preparse {
            names: collector.names,
            skipped,
        })
    })
}

/// The preparse tree of the functions inside the function whose span starts
/// at `start`, about to become a lazy record: the outermost functions it
/// may skip, each with the facts `capture` found for it or, for one this
/// compile already skipped, the facts `skips` recorded. `None` when it has
/// nothing to skip.
pub(crate) fn collect(
    capture: &CaptureFacts,
    skips: Option<&SkipTable>,
    start: u32,
) -> Option<Box<Preparse>> {
    let node = capture.function_node(start)?;
    let mut collector = Collector::new(capture, skips);
    let mut skipped = Vec::new();
    collector.gather(node, &mut skipped);
    (!skipped.is_empty()).then(|| {
        Box::new(Preparse {
            names: collector.names,
            skipped,
        })
    })
}

/// The statements a skip would blank in `body`, when long enough to be
/// worth it.
pub(crate) fn blank_range(body: &FunctionBody<'_>) -> Option<(u32, u32)> {
    let first = body.statements.first()?;
    let range = (
        oxc_span::GetSpan::span(first).start,
        body.span.end.checked_sub(1)?,
    );
    (range.1 >= range.0 + MIN_SKIPPED_BODY).then_some(range)
}

struct Collector<'c> {
    capture: &'c CaptureFacts,
    skips: Option<&'c SkipTable>,
    names: Vec<String>,
    capture_ids: FxHashMap<u32, u32>,
    spellings: FxHashMap<String, u32>,
}

impl<'c> Collector<'c> {
    fn new(capture: &'c CaptureFacts, skips: Option<&'c SkipTable>) -> Self {
        Self {
            capture,
            skips,
            names: Vec::new(),
            capture_ids: FxHashMap::default(),
            spellings: FxHashMap::default(),
        }
    }

    fn spelling(&mut self, name: &str) -> u32 {
        if let Some(&id) = self.spellings.get(name) {
            return id;
        }
        let id = u32::try_from(self.names.len()).expect("preparse names fit u32");
        self.names.push(name.to_owned());
        self.spellings.insert(name.to_owned(), id);
        id
    }

    fn capture_name(&mut self, id: u32) -> u32 {
        if let Some(&local) = self.capture_ids.get(&id) {
            return local;
        }
        let local = self.spelling(self.capture.name(id));
        self.capture_ids.insert(id, local);
        local
    }

    fn sorted(&mut self, ids: Vec<u32>) -> Vec<u32> {
        let mut local: Vec<u32> = ids.into_iter().map(|id| self.capture_name(id)).collect();
        local.sort_unstable();
        local
    }

    /// The skippable functions under `node`, looking through the ones that
    /// cannot be skipped.
    fn gather(&mut self, node: &FunctionNode, out: &mut Vec<SkippedFunction>) {
        for &index in &node.children {
            let child = self.capture.function_at(index);
            if child.recorded {
                if let Some(facts) = self.skips.and_then(|skips| skips.get(child.span.0)) {
                    let adopted = self.adopt(facts);
                    out.push(adopted);
                }
                continue;
            }
            let Some(blank) = child.blank else {
                self.gather(child, out);
                continue;
            };
            let scope = self.capture.node_scope(child);
            let escape: Vec<u32> = scope.escape().collect();
            let escape_writes: Vec<u32> = scope.escape_writes().collect();
            let escape_eval = scope.escape_eval();
            let contains_eval = self.capture.contains_eval(scope);
            let mut function = SkippedFunction {
                span: child.span,
                blank,
                escape: self.sorted(escape),
                escape_writes: self.sorted(escape_writes),
                escape_eval,
                contains_eval,
                children: Vec::new(),
            };
            let mut children = Vec::new();
            self.gather(child, &mut children);
            function.children = children;
            out.push(function);
        }
    }

    /// Re-spell a recorded skipped function into this tree's names.
    fn adopt(&mut self, facts: SkippedFacts<'_>) -> SkippedFunction {
        let function = facts.function;
        let mut respell = |ids: &[u32]| {
            let mut local: Vec<u32> = ids
                .iter()
                .map(|&id| self.spelling(&facts.names[id as usize]))
                .collect();
            local.sort_unstable();
            local
        };
        let escape = respell(&function.escape);
        let escape_writes = respell(&function.escape_writes);
        let children = function
            .children
            .iter()
            .map(|child| {
                self.adopt(SkippedFacts {
                    names: facts.names,
                    function: child,
                })
            })
            .collect();
        SkippedFunction {
            span: function.span,
            blank: function.blank,
            escape,
            escape_writes,
            escape_eval: function.escape_eval,
            contains_eval: function.contains_eval,
            children,
        }
    }
}
