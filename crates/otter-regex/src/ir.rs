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

use crate::classes::CodePointSet;
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

    let has_backref = e.insns.iter().any(|i| matches!(i, Insn::BackRef { .. }));
    let loop_marks = e.next_mark - mark_base;
    let first_set = compute_first_set(&e.insns, flags.ignore_case);
    Program {
        insns: e.insns,
        group_count: parsed.group_count,
        group_names: parsed.group_names,
        has_backref,
        multiline: flags.multiline,
        ignore_case: flags.ignore_case,
        unicode: flags.is_unicode_mode(),
        loop_marks,
        first_set,
    }
}

/// If the program must begin by consuming a single literal or non-negated class
/// (no anchor, no empty alternative), return the set of code points that can
/// start a match. Conservatively `None` under case-insensitivity (folding widens
/// the start set) or for any non-trivial leading instruction.
fn compute_first_set(insns: &[Insn], ignore_case: bool) -> Option<CodePointSet> {
    if ignore_case {
        return None;
    }
    let mut pc = 0;
    loop {
        match insns.get(pc)? {
            Insn::Save(_) | Insn::ClearCapture(_) => pc += 1,
            Insn::Char {
                cp,
                ignore_case: false,
            } => {
                let mut set = CodePointSet::new();
                set.insert(*cp);
                return Some(set);
            }
            Insn::Class {
                set,
                negate: false,
                ignore_case: false,
            } if set.strings.is_empty() => {
                return Some(set.code_points.clone());
            }
            _ => return None,
        }
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
            Node::Char { cp, ignore_case } => {
                self.emit(Insn::Char {
                    cp: *cp,
                    ignore_case: *ignore_case,
                });
            }
            Node::AnyChar { dot_all } => {
                self.emit(Insn::AnyChar { dot_all: *dot_all });
            }
            Node::Class {
                set,
                negate,
                ignore_case,
            } => {
                if set.strings.is_empty() {
                    self.emit(Insn::Class {
                        set: set.clone(),
                        negate: *negate,
                        ignore_case: *ignore_case,
                    });
                } else {
                    // A `v`-mode class with string alternatives matches a
                    // variable-length input, so lower it as an alternation
                    // `(?: s1 | s2 | … | [codePoints] )`. Longer strings are
                    // tried first (§22.2.2 match priority); the parser has
                    // already rejected a negated string-bearing class.
                    let mut strings = set.strings.clone();
                    strings.sort_by_key(|s| std::cmp::Reverse(s.len()));
                    let mut alts: Vec<Node> = Vec::with_capacity(strings.len() + 1);
                    for s in strings {
                        let seq: Vec<Node> = s
                            .iter()
                            .map(|cp| Node::Char {
                                cp: *cp,
                                ignore_case: *ignore_case,
                            })
                            .collect();
                        alts.push(if seq.is_empty() {
                            Node::Empty
                        } else {
                            Node::Concat(seq)
                        });
                    }
                    if !set.code_points.is_empty() {
                        alts.push(Node::Class {
                            set: crate::classes::ClassSet::from_code_points(
                                set.code_points.clone(),
                            ),
                            negate: false,
                            ignore_case: *ignore_case,
                        });
                    }
                    self.compile_alternation(&alts);
                }
            }
            Node::Assert(a) => {
                let insn = match a {
                    Assertion::StartOfLine { multiline } => Insn::AssertStart {
                        multiline: *multiline,
                    },
                    Assertion::EndOfLine { multiline } => Insn::AssertEnd {
                        multiline: *multiline,
                    },
                    Assertion::WordBoundary { invert } => Insn::WordBoundary(*invert),
                };
                self.emit(insn);
            }
            Node::BackRef { index, ignore_case } => {
                self.emit(Insn::BackRef {
                    index: *index,
                    ignore_case: *ignore_case,
                });
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
        let captures = capture_indices(node);
        for _ in 0..min {
            self.clear_captures(&captures);
            self.compile(node);
        }
        match max {
            None => self.compile_star(node, greedy, &captures),
            Some(max) => self.compile_bounded(node, max - min, greedy, &captures),
        }
    }

    /// `e*` (greedy or lazy) appended after any mandatory copies. A loop-mark
    /// guards against an empty-matching body re-iterating forever
    /// (§22.2.2.5.1): each iteration records its start position and fails the
    /// back-edge if the body consumed nothing.
    fn compile_star(&mut self, node: &Node, greedy: bool, captures: &[u32]) {
        let mark = self.new_mark();
        let head = self.emit(Insn::SetMark(mark));
        let split = self.emit(Insn::Split(0, 0));
        let body = self.here();
        self.clear_captures(captures);
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
    fn compile_bounded(&mut self, node: &Node, count: u32, greedy: bool, captures: &[u32]) {
        let mut splits = Vec::with_capacity(count as usize);
        for _ in 0..count {
            splits.push(self.emit(Insn::Split(0, 0)));
            self.clear_captures(captures);
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

    fn clear_captures(&mut self, captures: &[u32]) {
        for index in captures {
            self.emit(Insn::ClearCapture(*index));
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

fn capture_indices(node: &Node) -> Vec<u32> {
    fn visit(node: &Node, out: &mut Vec<u32>) {
        match node {
            Node::Group { kind, body } => {
                if let GroupKind::Capturing { index, .. } = kind {
                    out.push(*index);
                }
                visit(body, out);
            }
            Node::Concat(nodes) | Node::Alternate(nodes) => {
                for node in nodes {
                    visit(node, out);
                }
            }
            Node::Repeat { node, .. } => visit(node, out),
            Node::Empty
            | Node::Char { .. }
            | Node::AnyChar { .. }
            | Node::Class { .. }
            | Node::Assert(_)
            | Node::BackRef { .. } => {}
        }
    }

    let mut out = Vec::new();
    visit(node, &mut out);
    out
}
