//! Lowering from the parsed AST to the executable [`Program`].
//!
//! Walks the [`crate::parser::ast::Node`] tree and emits the flat [`Insn`]
//! vector: capturing groups become `Save` pairs, alternation becomes
//! `Split`/`Jump` chains, and quantifiers desugar into split loops (unbounded)
//! or bounded optional chains (counted). Lookaround bodies are appended inline,
//! guarded by a `Jump` so ordinary control flow never enters them.
//!
//! # Contents
//! - [`lower`] — AST + capture metadata + flags → [`Program`].
//!
//! # Invariants
//! - Greedy quantifiers emit splits preferring the repeat branch; lazy
//!   quantifiers prefer the exit branch (§22.2.2 match priority).
//! - Counted repetition is bounded by the parser's `MAX_REPEAT`, so expansion
//!   cannot blow up the instruction vector.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-pattern-matching> (§22.2.2)

use crate::flags::Flags;
use crate::parser::Parsed;
use crate::parser::ast::{Assertion, GroupKind, Node, Quantifier};
use crate::program::{Insn, Program};

/// Lower a parsed pattern into an executable program.
pub(crate) fn lower(parsed: Parsed, flags: Flags) -> Program {
    // Loop-mark slots follow the capture slots in the shared slot vector.
    let mark_base = 2 * (parsed.group_count as usize + 1);
    let mut e = Emitter {
        insns: Vec::new(),
        mark_base,
        next_mark: mark_base,
    };
    e.emit(Insn::Save(0));
    e.compile(&parsed.root);
    e.emit(Insn::Save(1));
    e.emit(Insn::Match);

    let has_backref = e.insns.iter().any(|i| matches!(i, Insn::BackRef(_)));
    let loop_marks = e.next_mark - mark_base;
    Program {
        insns: e.insns,
        group_count: parsed.group_count,
        group_names: parsed.group_names,
        has_backref,
        multiline: flags.multiline,
        ignore_case: flags.ignore_case,
        unicode: flags.is_unicode_mode(),
        loop_marks,
    }
}

struct Emitter {
    insns: Vec<Insn>,
    /// First loop-mark slot index (just past the capture slots).
    mark_base: usize,
    /// Next free loop-mark slot.
    next_mark: usize,
}

impl Emitter {
    fn emit(&mut self, insn: Insn) -> usize {
        let at = self.insns.len();
        self.insns.push(insn);
        at
    }

    fn here(&self) -> usize {
        self.insns.len()
    }

    fn new_mark(&mut self) -> usize {
        let m = self.next_mark;
        self.next_mark += 1;
        m
    }

    fn compile(&mut self, node: &Node) {
        match node {
            Node::Empty => {}
            Node::Char(c) => {
                self.emit(Insn::Char(*c));
            }
            Node::AnyChar { dot_all } => {
                self.emit(Insn::AnyChar { dot_all: *dot_all });
            }
            Node::Class { set, negate } => {
                self.emit(Insn::Class {
                    set: set.clone(),
                    negate: *negate,
                });
            }
            Node::Assert(a) => {
                let insn = match a {
                    Assertion::StartOfLine => Insn::AssertStart,
                    Assertion::EndOfLine => Insn::AssertEnd,
                    Assertion::WordBoundary { invert } => Insn::WordBoundary(*invert),
                };
                self.emit(insn);
            }
            Node::BackRef { index } => {
                self.emit(Insn::BackRef(*index));
            }
            Node::Concat(nodes) => {
                for n in nodes {
                    self.compile(n);
                }
            }
            Node::Alternate(alts) => self.compile_alternation(alts),
            Node::Repeat { node, quant } => self.compile_repeat(node, *quant),
            Node::Group { kind, body } => self.compile_group(kind, body),
        }
    }

    fn compile_alternation(&mut self, alts: &[Node]) {
        if alts.len() == 1 {
            self.compile(&alts[0]);
            return;
        }
        let mut exit_jumps = Vec::new();
        for (i, alt) in alts.iter().enumerate() {
            if i + 1 < alts.len() {
                let split = self.emit(Insn::Split(0, 0));
                let a_start = self.here();
                self.compile(alt);
                exit_jumps.push(self.emit(Insn::Jump(0)));
                let b_start = self.here();
                self.insns[split] = Insn::Split(a_start, b_start);
            } else {
                self.compile(alt);
            }
        }
        let end = self.here();
        for j in exit_jumps {
            self.insns[j] = Insn::Jump(end);
        }
    }

    fn compile_repeat(&mut self, node: &Node, quant: Quantifier) {
        let Quantifier { min, max, greedy } = quant;
        for _ in 0..min {
            self.compile(node);
        }
        match max {
            None => self.compile_star(node, greedy),
            Some(max) => self.compile_bounded(node, max - min, greedy),
        }
    }

    /// `e*` (greedy or lazy) appended after any mandatory copies. A loop-mark
    /// guards against an empty-matching body re-iterating forever
    /// (§22.2.2.5.1): each iteration records its start position and fails the
    /// back-edge if the body consumed nothing.
    fn compile_star(&mut self, node: &Node, greedy: bool) {
        let mark = self.new_mark();
        let head = self.emit(Insn::SetMark(mark));
        let split = self.emit(Insn::Split(0, 0));
        let body = self.here();
        self.compile(node);
        self.emit(Insn::CheckProgress(mark));
        self.emit(Insn::Jump(head));
        let out = self.here();
        self.insns[split] = if greedy {
            Insn::Split(body, out)
        } else {
            Insn::Split(out, body)
        };
    }

    /// `count` optional copies of `node` that may each bail to the end.
    fn compile_bounded(&mut self, node: &Node, count: u32, greedy: bool) {
        let mut splits = Vec::with_capacity(count as usize);
        for _ in 0..count {
            splits.push(self.emit(Insn::Split(0, 0)));
            self.compile(node);
        }
        let end = self.here();
        for s in splits {
            let body = s + 1;
            self.insns[s] = if greedy {
                Insn::Split(body, end)
            } else {
                Insn::Split(end, body)
            };
        }
    }

    fn compile_group(&mut self, kind: &GroupKind, body: &Node) {
        match kind {
            GroupKind::Capturing { index, .. } => {
                let slot = 2 * (*index as usize);
                self.emit(Insn::Save(slot));
                self.compile(body);
                self.emit(Insn::Save(slot + 1));
            }
            GroupKind::NonCapturing => self.compile(body),
            GroupKind::Lookahead { negate } => self.compile_look(false, *negate, body),
            GroupKind::Lookbehind { negate } => self.compile_look(true, *negate, body),
        }
    }

    fn compile_look(&mut self, behind: bool, negate: bool, body: &Node) {
        let look = self.emit(Insn::Look {
            negate,
            behind,
            entry: 0,
        });
        let jump = self.emit(Insn::Jump(0));
        let entry = self.here();
        self.compile(body);
        self.emit(Insn::LookMatch);
        let after = self.here();
        self.insns[look] = Insn::Look {
            negate,
            behind,
            entry,
        };
        self.insns[jump] = Insn::Jump(after);
    }
}
