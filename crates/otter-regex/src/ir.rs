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

use crate::classes::{ClassSet, CodePointSet};
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
        classes: Vec::new(),
        next_mark: mark_base,
        unicode: flags.is_unicode_mode(),
    };
    // The overall-match bounds (slots 0/1) are not emitted as `Save`
    // instructions: the executor seeds slot 0 with the start offset before it
    // runs, and the terminating `Match` records slot 1. This removes two
    // instruction dispatches from every match.
    e.compile(&parsed.root);
    e.emit(Insn::Match);

    // Precompute each class's ASCII membership bitmap so the executor tests the
    // dominant ASCII range with one bit check instead of a binary search.
    for set in &mut e.classes {
        set.finalize_ascii();
    }

    let loop_marks = e.next_mark - mark_base;
    let unicode = flags.is_unicode_mode();
    let prefilter =
        compute_first_set(&e.insns, &e.classes, flags.ignore_case, unicode).map(|(set, canon)| {
            if canon {
                crate::program::Prefilter::from_set_canon(&set, unicode)
            } else {
                crate::program::Prefilter::from_set(&set)
            }
        });
    let names: std::sync::Arc<[String]> = parsed
        .group_names
        .into_iter()
        .map(Option::unwrap_or_default)
        .collect();
    Program {
        insns: e.insns,
        classes: e.classes,
        group_count: parsed.group_count,
        names,
        unicode,
        loop_marks,
        prefilter,
    }
}

/// If every match must begin by consuming a single literal or non-negated class
/// — including a leading alternation of such (`foo|bar`) or a leading quantified
/// atom whose start is still characterizable — return the code points that can
/// start a match plus whether the scan must canonicalize the input first.
///
/// The boolean is `true` when any leading atom matches case-insensitively
/// (`i` flag, or an inline `(?i:` modifier); in that case the returned set holds
/// the *canonicalized* member code points so the scan can fold each input code
/// point and compare, mirroring `char_eq` exactly. `None` for any
/// non-characterizable leading instruction (anchor, `.`, backref, lookaround, or
/// a path that can match the empty string), or a leading class too large for
/// canonicalization to pay off.
fn compute_first_set(
    insns: &[Insn],
    classes: &[crate::classes::ClassSet],
    ignore_case: bool,
    unicode: bool,
) -> Option<(CodePointSet, bool)> {
    // BFS over the zero-width-passable prefix: follow control flow until the
    // first consuming instruction on every path. Every reachable consuming
    // instruction must be a literal or non-negated class, or the whole filter
    // is unsound (could skip a valid start) and we bail.
    let mut out = CodePointSet::new();
    let mut needs_canon = ignore_case;
    let mut visited = vec![false; insns.len()];
    let mut work = vec![0usize];
    while let Some(pc) = work.pop() {
        if pc >= insns.len() || visited[pc] {
            continue;
        }
        visited[pc] = true;
        match &insns[pc] {
            // Zero-width control flow / bookkeeping: pass through.
            Insn::Save(_) | Insn::ClearCapture(_) | Insn::SetMark(_) | Insn::CheckProgress(_) => {
                work.push(pc + 1)
            }
            Insn::Jump(t) => work.push(*t),
            Insn::Split(a, b) => {
                work.push(*a);
                work.push(*b);
            }
            // First consuming instruction on this path: must be characterizable.
            // A case-insensitive atom forces canonicalization of the whole set.
            Insn::Char { cp, ignore_case } => {
                needs_canon |= *ignore_case;
                out.insert(*cp);
            }
            // A fused literal run starts with its first unit.
            Insn::CharSeq(seq) => out.insert(u32::from(seq[0])),
            Insn::Class {
                class,
                negate: false,
                ignore_case,
            } if classes[*class as usize].strings.is_empty() => {
                needs_canon |= *ignore_case;
                out.union_with(&classes[*class as usize].code_points);
            }
            // A fused repeat's atom is the first thing matched. Contribute its
            // start set; if it may match zero times (`min == 0`), also follow
            // through to whatever can begin after it.
            Insn::Repeat { atom, min, .. } => {
                use crate::program::RepeatAtom;
                match atom {
                    RepeatAtom::Char { cp, ignore_case } => {
                        needs_canon |= *ignore_case;
                        out.insert(*cp);
                    }
                    RepeatAtom::Class {
                        class,
                        negate: false,
                        ignore_case,
                    } if classes[*class as usize].strings.is_empty() => {
                        needs_canon |= *ignore_case;
                        out.union_with(&classes[*class as usize].code_points);
                    }
                    // Negated class or `.` — uncharacterizable start.
                    _ => return None,
                }
                if *min == 0 {
                    work.push(pc + 1);
                }
            }
            // Anything else (anchor, `.`, backref, lookaround, negated/string
            // class, or `Match` reachable zero-width) makes the start set
            // unsound.
            _ => return None,
        }
    }
    if out.is_empty() {
        return None;
    }
    if needs_canon {
        out = canonicalize_set(&out, unicode)?;
        if out.is_empty() {
            return None;
        }
    }
    Some((out, needs_canon))
}

/// Fold every member of `set` to its canonical form (`fold_unicode` under `u`/
/// `v`, else `canonicalize`). Returns `None` when the set is too large for the
/// per-code-point fold to be worth it — such a prefilter would barely filter.
fn canonicalize_set(set: &CodePointSet, unicode: bool) -> Option<CodePointSet> {
    const MAX_FOLD: u32 = 4096;
    let mut count: u32 = 0;
    for r in set.ranges() {
        count = count.saturating_add(*r.end() - *r.start() + 1);
        if count > MAX_FOLD {
            return None;
        }
    }
    let mut out = CodePointSet::new();
    for r in set.ranges() {
        for cp in *r.start()..=*r.end() {
            let c = if unicode {
                crate::casefold::fold_unicode(cp)
            } else {
                crate::casefold::canonicalize(cp)
            };
            out.insert(c);
        }
    }
    Some(out)
}

struct Emitter {
    insns: Vec<Insn>,
    /// Out-of-line class sets; an `Insn::Class` stores an index into this.
    classes: Vec<ClassSet>,
    /// Next free loop-mark slot.
    next_mark: usize,
    /// `u`/`v` mode — disables the fused single-unit repeat (atoms are
    /// variable-width under surrogate-pair traversal).
    unicode: bool,
}

impl Emitter {
    fn emit(&mut self, insn: Insn) -> usize {
        let at = self.insns.len();
        self.insns.push(insn);
        at
    }

    /// Emit a pending literal run: nothing for an empty run, a single `Char` for
    /// one unit, a fused `CharSeq` for two or more. Clears the run.
    fn flush_char_run(&mut self, run: &mut Vec<u16>) {
        match run.len() {
            0 => {}
            1 => {
                self.emit(Insn::Char {
                    cp: u32::from(run[0]),
                    ignore_case: false,
                });
            }
            _ => {
                self.emit(Insn::CharSeq(run.as_slice().into()));
            }
        }
        run.clear();
    }

    /// Store a class set out of line, returning its index for an `Insn::Class`.
    fn intern_class(&mut self, set: ClassSet) -> u32 {
        let idx = self.classes.len() as u32;
        self.classes.push(set);
        idx
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
                    let class = self.intern_class(set.clone());
                    self.emit(Insn::Class {
                        class,
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
            Node::BackRef {
                indices,
                ignore_case,
            } => {
                self.emit(Insn::BackRef {
                    indices: indices.clone().into_boxed_slice(),
                    ignore_case: *ignore_case,
                });
            }
            Node::Concat(nodes) => {
                // Fuse a run of consecutive case-sensitive BMP literals into one
                // `CharSeq` so the matcher confirms them in a single slice
                // comparison rather than one dispatch per character. A
                // quantified literal is a `Repeat`/`Split` node, not a bare
                // `Char`, so only genuine fixed runs collect here.
                let mut run: Vec<u16> = Vec::new();
                for n in nodes {
                    if let Node::Char {
                        cp,
                        ignore_case: false,
                    } = n
                        && *cp < 0x1_0000
                        && !(0xD800..=0xDFFF).contains(cp)
                    {
                        run.push(*cp as u16);
                        continue;
                    }
                    self.flush_char_run(&mut run);
                    self.compile(n);
                }
                self.flush_char_run(&mut run);
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
        // Fuse an unbounded repeat of a single one-code-unit atom into one
        // instruction so the matcher consumes it in a tight loop. Only in
        // non-Unicode mode, where every atom is exactly one code unit.
        if max.is_none()
            && !self.unicode
            && let Some(atom) = self.fuseable_atom(node)
        {
            self.emit(Insn::Repeat { atom, min, greedy });
            return;
        }
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

    /// The fused-repeat atom for `node`, when it is a single one-code-unit atom
    /// (a literal, `.`, or a string-free class). Interns the class set if any.
    fn fuseable_atom(&mut self, node: &Node) -> Option<crate::program::RepeatAtom> {
        use crate::program::RepeatAtom;
        match node {
            Node::Char { cp, ignore_case } => Some(RepeatAtom::Char {
                cp: *cp,
                ignore_case: *ignore_case,
            }),
            Node::AnyChar { dot_all } => Some(RepeatAtom::Any { dot_all: *dot_all }),
            Node::Class {
                set,
                negate,
                ignore_case,
            } if set.strings.is_empty() => {
                let class = self.intern_class(set.clone());
                Some(RepeatAtom::Class {
                    class,
                    negate: *negate,
                    ignore_case: *ignore_case,
                })
            }
            _ => None,
        }
    }

    /// `e*` (greedy or lazy) appended after any mandatory copies. A loop-mark
    /// guards against an empty-matching body re-iterating forever
    /// (§22.2.2.5.1): each iteration records its start position and fails the
    /// back-edge if the body consumed nothing.
    ///
    /// When the body provably consumes at least one code point every iteration,
    /// that guard is unnecessary, so the loop drops the per-iteration `SetMark`
    /// and `CheckProgress` (and the mark slot) — two fewer instruction
    /// dispatches per matched character on the hot `\w+` / `[0-9]+` shape.
    fn compile_star(&mut self, node: &Node, greedy: bool, captures: &[u32]) {
        if consumes_input(node) {
            // Trailing-split loop: an entry split allows zero iterations, and a
            // second split *after* the body decides whether to re-iterate. This
            // keeps the back-edge a single `Split` instead of a `Jump` back to a
            // leading split, so each matched character pays one control dispatch
            // (the trailing split) plus the body — no separate jump.
            let entry = self.emit(Insn::Split(0, 0));
            let body = self.here();
            self.clear_captures(captures);
            self.compile(node);
            let back = self.emit(Insn::Split(0, 0));
            let out = self.here();
            let split = if greedy {
                Insn::Split(body, out)
            } else {
                Insn::Split(out, body)
            };
            self.insns[entry] = split.clone();
            self.insns[back] = split;
            return;
        }
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

/// Whether `node` provably consumes at least one code point on every successful
/// match. Conservative: returns `false` whenever unsure, so an unbounded-loop
/// progress guard is only ever dropped when an empty iteration is impossible.
fn consumes_input(node: &Node) -> bool {
    match node {
        Node::Char { .. } | Node::AnyChar { .. } => true,
        // A class always consumes one code point unless it carries an empty
        // string alternative (`v`-mode `\q{}`).
        Node::Class { set, .. } => set.strings.is_empty(),
        Node::Group { kind, body } => match kind {
            GroupKind::Capturing { .. } | GroupKind::NonCapturing => consumes_input(body),
            // Lookarounds are zero-width.
            GroupKind::Lookahead { .. } | GroupKind::Lookbehind { .. } => false,
        },
        // A concatenation consumes if any element always consumes.
        Node::Concat(nodes) => nodes.iter().any(consumes_input),
        // An alternation consumes only if every branch always consumes.
        Node::Alternate(nodes) => !nodes.is_empty() && nodes.iter().all(consumes_input),
        // `e{n,…}` consumes iff `n >= 1` and `e` consumes; otherwise it may
        // match the empty string.
        Node::Repeat { node, quant } => quant.min >= 1 && consumes_input(node),
        Node::Empty | Node::Assert(_) | Node::BackRef { .. } => false,
    }
}
