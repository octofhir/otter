//! Recursive-descent parser for the ECMAScript Pattern grammar (§22.2.1).
//!
//! Parses a UTF-16 pattern into the [`ast::Node`] tree, enforcing an **explicit
//! recursion-depth limit** so a pathological nested pattern raises
//! [`RegexError`] rather than overflowing the stack (the concrete failure the
//! forked engine exhibited on ~200 nested groups). A pre-scan assigns capture
//! indices and collects group names up front so numeric and named
//! backreferences resolve even when they appear before their group.
//!
//! # Contents
//! - [`ast`] — the syntax-tree node type.
//! - [`parse`] — entry point: pattern + flags → AST + capture-group metadata.
//! - [`MAX_NESTING_DEPTH`] — the recursion-depth guard.
//!
//! # Invariants
//! - The parser never recurses deeper than [`MAX_NESTING_DEPTH`] frames.
//! - Capture-group ids are assigned in source order, 1-based.
//! - Counted quantifiers above [`MAX_REPEAT`] are rejected, keeping the
//!   lowering output bounded.
//!
//! # Scope
//! Inline modifier groups `(?ims-ims:...)` (§22.2.1 RegularExpressionModifiers)
//! scope the `i`/`m`/`s` flags lexically to the group body; the effective flags
//! are stamped onto each `Char` / `Class` / `BackRef` / anchor node so the
//! matcher applies them per node. Still unsupported (raise a clear
//! [`RegexError`]): `\q{...}` and `v`-flag set operations.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-patterns> (§22.2.1)

pub(crate) mod ast;

use std::collections::BTreeSet;

use crate::classes::{ClassSet, CodePointSet};
use crate::error::RegexError;
use crate::flags::Flags;
use ast::{Assertion, GroupKind, Node, Quantifier};

/// Maximum nesting depth for groups / classes before the parser bails with
/// [`RegexError::TooDeep`]. Each level consumes several recursive-descent stack
/// frames, so this is kept well under the native stack budget; deeply nested
/// patterns raise a clean error instead of overflowing (the failure mode the
/// forked engine exhibited on ~200 nested groups). Raising this safely is a
/// later-phase task (an explicit work-stack or a large dedicated parse stack).
pub(crate) const MAX_NESTING_DEPTH: usize = 200;

/// Maximum counted-quantifier expansion the lowering accepts, bounding how far
/// `{n,m}` can inflate the instruction vector.
pub(crate) const MAX_REPEAT: u32 = 100_000;

/// The result of a successful parse: the AST plus capture-group bookkeeping the
/// lowering and executor need.
#[derive(Debug)]
pub(crate) struct Parsed {
    /// The root pattern node.
    pub(crate) root: Node,
    /// Total number of capturing groups (group 0 excluded).
    pub(crate) group_count: u32,
    /// Capture-group names in source order; `None` for unnamed groups.
    pub(crate) group_names: Vec<Option<String>>,
}

/// Parse `pattern` (UTF-16 code units) under `flags` into an AST.
pub(crate) fn parse(pattern: &[u16], flags: Flags) -> Result<Parsed, RegexError> {
    let mut p = Parser::new(pattern, flags);
    p.prescan()?;
    let root = p.parse_disjunction()?;
    if p.pos != p.units.len() {
        // The only thing that halts disjunction before end-of-input is an
        // unmatched `)`.
        return Err(p.err("unmatched ')'"));
    }
    validate_duplicate_named_groups_by_path(&root)?;
    Ok(Parsed {
        root,
        group_count: p.total_groups,
        group_names: p.group_names,
    })
}

fn validate_duplicate_named_groups_by_path(root: &Node) -> Result<(), RegexError> {
    let initial = vec![BTreeSet::new()];
    validate_names(root, initial).map(|_| ())
}

fn validate_names(
    node: &Node,
    states: Vec<BTreeSet<String>>,
) -> Result<Vec<BTreeSet<String>>, RegexError> {
    match node {
        Node::Empty
        | Node::Char { .. }
        | Node::AnyChar { .. }
        | Node::Class { .. }
        | Node::Assert(_)
        | Node::BackRef { .. } => Ok(states),
        Node::Concat(nodes) => {
            let mut current = states;
            for node in nodes {
                current = validate_names(node, current)?;
            }
            Ok(current)
        }
        Node::Alternate(alts) => {
            let mut out = Vec::new();
            for alt in alts {
                out.extend(validate_names(alt, states.clone())?);
            }
            Ok(out)
        }
        Node::Repeat { node, .. } => validate_names(node, states),
        Node::Group { kind, body } => {
            let mut states = states;
            if let GroupKind::Capturing {
                name: Some(name), ..
            } = kind
            {
                for state in &mut states {
                    if !state.insert(name.clone()) {
                        return Err(RegexError::Syntax {
                            message: "duplicate named capturing group in the same alternative"
                                .to_string(),
                            offset: 0,
                        });
                    }
                }
            }
            validate_names(body, states)
        }
    }
}

struct Parser<'a> {
    units: &'a [u16],
    pos: usize,
    flags: Flags,
    depth: usize,
    /// Total capturing groups, filled by [`Parser::prescan`].
    total_groups: u32,
    /// Names indexed by `group - 1`, filled by [`Parser::prescan`].
    group_names: Vec<Option<String>>,
    /// Next capture index to assign during the main parse (1-based).
    next_group: u32,
}

impl<'a> Parser<'a> {
    fn new(units: &'a [u16], flags: Flags) -> Self {
        Self {
            units,
            pos: 0,
            flags,
            depth: 0,
            total_groups: 0,
            group_names: Vec::new(),
            next_group: 0,
        }
    }

    /// Build a literal-character node, stamping the `i` flag effective at
    /// the current parse position (so an inline `(?i:...)` / `(?-i:...)`
    /// modifier scopes the case-insensitive comparison).
    fn char_node(&self, cp: u32) -> Node {
        Node::Char {
            cp,
            ignore_case: self.flags.ignore_case,
        }
    }

    /// Build a character-class node, stamping the effective `i` flag.
    fn class_node(&self, set: ClassSet, negate: bool) -> Node {
        Node::Class {
            set,
            negate,
            ignore_case: self.flags.ignore_case,
        }
    }

    /// Build a backreference node, stamping the effective `i` flag.
    fn backref_node(&self, indices: Vec<u32>) -> Node {
        Node::BackRef {
            indices,
            ignore_case: self.flags.ignore_case,
        }
    }

    fn unicode(&self) -> bool {
        self.flags.is_unicode_mode()
    }

    fn err(&self, msg: &str) -> RegexError {
        RegexError::Syntax {
            message: msg.to_string(),
            offset: self.pos,
        }
    }

    fn peek(&self) -> Option<u16> {
        self.units.get(self.pos).copied()
    }

    fn peek_at(&self, off: usize) -> Option<u16> {
        self.units.get(self.pos + off).copied()
    }

    fn eat(&mut self, ch: u16) -> bool {
        if self.peek() == Some(ch) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    // --- Pre-scan: capture indices and names ---------------------------------

    /// Walk the pattern once to count capturing groups and record names, so
    /// forward references resolve. Parens inside character classes and after a
    /// backslash are ignored.
    fn prescan(&mut self) -> Result<(), RegexError> {
        let n = self.units.len();
        let mut i = 0;
        let mut in_class = false;
        let mut count: u32 = 0;
        let mut names: Vec<Option<String>> = Vec::new();
        while i < n {
            let c = self.units[i];
            if c == b'\\' as u16 {
                i += 2;
                continue;
            }
            if in_class {
                if c == b']' as u16 {
                    in_class = false;
                }
                i += 1;
                continue;
            }
            if c == b'[' as u16 {
                in_class = true;
                i += 1;
                continue;
            }
            if c == b'(' as u16 {
                if self.units.get(i + 1) == Some(&(b'?' as u16)) {
                    // `(?<name>` is a named capture; `(?<=` / `(?<!` are
                    // lookbehind, not captures.
                    let is_named = self.units.get(i + 2) == Some(&(b'<' as u16))
                        && self.units.get(i + 3) != Some(&(b'=' as u16))
                        && self.units.get(i + 3) != Some(&(b'!' as u16));
                    if is_named {
                        count += 1;
                        let (name, next) = read_group_name(self.units, i + 3, self.unicode())
                            .ok_or_else(|| RegexError::Syntax {
                                message: "invalid group name".to_string(),
                                offset: i + 3,
                            })?;
                        names.push(Some(name));
                        i = next;
                        continue;
                    }
                    i += 1;
                } else {
                    count += 1;
                    names.push(None);
                    i += 1;
                }
                continue;
            }
            i += 1;
        }
        self.total_groups = count;
        self.group_names = names;
        Ok(())
    }

    fn name_to_indices(&self, name: &str) -> Vec<u32> {
        self.group_names
            .iter()
            .enumerate()
            .filter_map(|(idx, n)| (n.as_deref() == Some(name)).then_some((idx + 1) as u32))
            .collect()
    }

    // --- Grammar -------------------------------------------------------------

    fn enter(&mut self) -> Result<(), RegexError> {
        self.depth += 1;
        if self.depth > MAX_NESTING_DEPTH {
            return Err(RegexError::TooDeep {
                limit: MAX_NESTING_DEPTH,
            });
        }
        Ok(())
    }

    fn leave(&mut self) {
        self.depth -= 1;
    }

    fn parse_disjunction(&mut self) -> Result<Node, RegexError> {
        self.enter()?;
        let mut alts = vec![self.parse_alternative()?];
        while self.eat(b'|' as u16) {
            alts.push(self.parse_alternative()?);
        }
        self.leave();
        if alts.len() == 1 {
            Ok(alts.pop().unwrap())
        } else {
            Ok(Node::Alternate(alts))
        }
    }

    fn parse_alternative(&mut self) -> Result<Node, RegexError> {
        let mut terms = Vec::new();
        while let Some(term) = self.parse_term()? {
            terms.push(term);
        }
        Ok(match terms.len() {
            0 => Node::Empty,
            1 => terms.pop().unwrap(),
            _ => Node::Concat(terms),
        })
    }

    /// One term: an atom plus an optional quantifier. Returns `None` at an
    /// alternative boundary (`|`, `)`, end of input).
    fn parse_term(&mut self) -> Result<Option<Node>, RegexError> {
        match self.peek() {
            None => Ok(None),
            Some(c) if c == b'|' as u16 || c == b')' as u16 => Ok(None),
            _ => {
                let atom = self.parse_atom()?;
                let atom = self.maybe_quantify(atom)?;
                Ok(Some(atom))
            }
        }
    }

    fn parse_atom(&mut self) -> Result<Node, RegexError> {
        let c = self.peek().expect("parse_atom at end of input");
        match c {
            x if x == b'^' as u16 => {
                self.pos += 1;
                Ok(Node::Assert(Assertion::StartOfLine {
                    multiline: self.flags.multiline,
                }))
            }
            x if x == b'$' as u16 => {
                self.pos += 1;
                Ok(Node::Assert(Assertion::EndOfLine {
                    multiline: self.flags.multiline,
                }))
            }
            x if x == b'.' as u16 => {
                self.pos += 1;
                Ok(Node::AnyChar {
                    dot_all: self.flags.dot_all,
                })
            }
            x if x == b'(' as u16 => self.parse_group(),
            x if x == b'[' as u16 => self.parse_class(),
            x if x == b'\\' as u16 => self.parse_escape_atom(),
            x if x == b'*' as u16 || x == b'+' as u16 || x == b'?' as u16 => {
                Err(self.err("nothing to repeat"))
            }
            x if x == b'{' as u16 => {
                // A `{` that does not begin a valid quantifier is a literal in
                // non-unicode mode, an error in unicode mode.
                if self.looks_like_quantifier() {
                    Err(self.err("nothing to repeat"))
                } else if self.unicode() {
                    Err(self.err("lone quantifier brace"))
                } else {
                    self.pos += 1;
                    Ok(self.char_node(u32::from(c)))
                }
            }
            x if x == b']' as u16 || x == b'}' as u16 => {
                if self.unicode() {
                    return Err(self.err("lone bracket"));
                }
                self.pos += 1;
                Ok(self.char_node(u32::from(c)))
            }
            _ => {
                let cp = self.read_pattern_codepoint();
                Ok(self.char_node(cp))
            }
        }
    }

    /// Read one code point from the pattern, combining a surrogate pair in
    /// unicode mode.
    fn read_pattern_codepoint(&mut self) -> u32 {
        let hi = self.units[self.pos];
        self.pos += 1;
        if self.unicode()
            && (0xD800..=0xDBFF).contains(&hi)
            && let Some(lo) = self.peek()
            && (0xDC00..=0xDFFF).contains(&lo)
        {
            self.pos += 1;
            return 0x10000 + ((u32::from(hi) - 0xD800) << 10) + (u32::from(lo) - 0xDC00);
        }
        u32::from(hi)
    }

    fn parse_group(&mut self) -> Result<Node, RegexError> {
        debug_assert_eq!(self.peek(), Some(b'(' as u16));
        self.pos += 1; // consume '('
        let kind = if self.eat(b'?' as u16) {
            match self.peek() {
                Some(x) if x == b':' as u16 => {
                    self.pos += 1;
                    GroupKind::NonCapturing
                }
                Some(x) if x == b'=' as u16 => {
                    self.pos += 1;
                    GroupKind::Lookahead { negate: false }
                }
                Some(x) if x == b'!' as u16 => {
                    self.pos += 1;
                    GroupKind::Lookahead { negate: true }
                }
                Some(x) if x == b'<' as u16 => {
                    match self.peek_at(1) {
                        Some(y) if y == b'=' as u16 => {
                            self.pos += 2;
                            GroupKind::Lookbehind { negate: false }
                        }
                        Some(y) if y == b'!' as u16 => {
                            self.pos += 2;
                            GroupKind::Lookbehind { negate: true }
                        }
                        _ => {
                            // `(?<name>` named capture.
                            let (name, next) =
                                read_group_name(self.units, self.pos + 1, self.unicode())
                                    .ok_or_else(|| self.err("invalid group name"))?;
                            self.pos = next;
                            self.next_group += 1;
                            GroupKind::Capturing {
                                index: self.next_group,
                                name: Some(name),
                            }
                        }
                    }
                }
                // §22.2.1 RegularExpressionModifiers — `(?ims-ims:...)`
                // scopes the i/m/s flags to the group body.
                Some(x)
                    if x == b'i' as u16
                        || x == b'm' as u16
                        || x == b's' as u16
                        || x == b'-' as u16 =>
                {
                    return self.parse_modifier_group();
                }
                _ => return Err(self.err("unsupported group modifier")),
            }
        } else {
            self.next_group += 1;
            GroupKind::Capturing {
                index: self.next_group,
                name: None,
            }
        };
        let body = self.parse_disjunction()?;
        if !self.eat(b')' as u16) {
            return Err(self.err("unterminated group"));
        }
        Ok(Node::Group {
            kind,
            body: Box::new(body),
        })
    }

    /// Parse a modifier group `(?AddModifiers-RemoveModifiers:Disjunction)`
    /// after the leading `(?` has been consumed. The `i`/`m`/`s` flags in
    /// `AddModifiers` are enabled and those in `RemoveModifiers` disabled
    /// for the body only; the flags are restored afterwards so the
    /// modifier scopes lexically (§22.2.1).
    ///
    /// Early errors (§22.2.1.1): a modifier letter may appear at most once
    /// across both lists, at least one letter must be present, and the
    /// dash form must carry at least one removed modifier.
    fn parse_modifier_group(&mut self) -> Result<Node, RegexError> {
        let saved = self.flags;
        let mut seen = [false; 3]; // i, m, s
        let idx = |c: u16| match c {
            x if x == b'i' as u16 => Some(0usize),
            x if x == b'm' as u16 => Some(1usize),
            x if x == b's' as u16 => Some(2usize),
            _ => None,
        };
        let mut add_count = 0usize;
        while let Some(c) = self.peek()
            && let Some(i) = idx(c)
        {
            if seen[i] {
                return Err(self.err("duplicate modifier flag"));
            }
            seen[i] = true;
            match i {
                0 => self.flags.ignore_case = true,
                1 => self.flags.multiline = true,
                _ => self.flags.dot_all = true,
            }
            add_count += 1;
            self.pos += 1;
        }
        let mut remove_count = 0usize;
        if self.eat(b'-' as u16) {
            while let Some(c) = self.peek()
                && let Some(i) = idx(c)
            {
                if seen[i] {
                    return Err(self.err("duplicate modifier flag"));
                }
                seen[i] = true;
                match i {
                    0 => self.flags.ignore_case = false,
                    1 => self.flags.multiline = false,
                    _ => self.flags.dot_all = false,
                }
                remove_count += 1;
                self.pos += 1;
            }
        }
        // At least one modifier must appear in total (`(?i-:...)` and
        // `(?-i:...)` are valid; `(?-:...)` is not).
        if add_count == 0 && remove_count == 0 {
            self.flags = saved;
            return Err(self.err("empty modifier group"));
        }
        if !self.eat(b':' as u16) {
            self.flags = saved;
            return Err(self.err("expected ':' in modifier group"));
        }
        let body = self.parse_disjunction()?;
        if !self.eat(b')' as u16) {
            self.flags = saved;
            return Err(self.err("unterminated group"));
        }
        self.flags = saved;
        Ok(Node::Group {
            kind: GroupKind::NonCapturing,
            body: Box::new(body),
        })
    }

    // --- Quantifiers ---------------------------------------------------------

    fn looks_like_quantifier(&self) -> bool {
        // `{` followed by at least one digit and a `,` or `}`.
        if self.peek() != Some(b'{' as u16) {
            return false;
        }
        let mut i = self.pos + 1;
        let mut saw_digit = false;
        while let Some(&c) = self.units.get(i) {
            if (b'0' as u16..=b'9' as u16).contains(&c) {
                saw_digit = true;
                i += 1;
            } else {
                break;
            }
        }
        if !saw_digit {
            return false;
        }
        match self.units.get(i).copied() {
            Some(c) if c == b'}' as u16 => true,
            Some(c) if c == b',' as u16 => true,
            _ => false,
        }
    }

    /// `true` when the cursor sits on a quantifier (`* + ?` or a valid
    /// `{n,m}` brace).
    fn peek_is_quantifier(&self) -> bool {
        match self.peek() {
            Some(c) if c == b'*' as u16 || c == b'+' as u16 || c == b'?' as u16 => true,
            Some(c) if c == b'{' as u16 => self.looks_like_quantifier(),
            _ => false,
        }
    }

    fn maybe_quantify(&mut self, atom: Node) -> Result<Node, RegexError> {
        // §22.2.1 — in unicode (`u`/`v`) mode a Quantifier may only follow an
        // Atom, never an Assertion. Annex B keeps the legacy
        // QuantifiableAssertion leniency (`(?=.)?`, `^*`) in non-unicode mode.
        if self.unicode() && is_assertion_node(&atom) && self.peek_is_quantifier() {
            return Err(self.err("quantifier may not follow an assertion in unicode mode"));
        }
        let (min, max) = match self.peek() {
            Some(c) if c == b'*' as u16 => {
                self.pos += 1;
                (0, None)
            }
            Some(c) if c == b'+' as u16 => {
                self.pos += 1;
                (1, None)
            }
            Some(c) if c == b'?' as u16 => {
                self.pos += 1;
                (0, Some(1))
            }
            Some(c) if c == b'{' as u16 && self.looks_like_quantifier() => {
                self.parse_brace_quantifier()?
            }
            _ => return Ok(atom),
        };
        let greedy = !self.eat(b'?' as u16);
        if let Some(m) = max
            && m < min
        {
            return Err(self.err("quantifier min exceeds max"));
        }
        if min > MAX_REPEAT || max.is_some_and(|m| m > MAX_REPEAT) {
            return Err(self.err("quantifier bound too large (phase 1 limit)"));
        }
        Ok(Node::Repeat {
            node: Box::new(atom),
            quant: Quantifier { min, max, greedy },
        })
    }

    fn parse_brace_quantifier(&mut self) -> Result<(u32, Option<u32>), RegexError> {
        debug_assert_eq!(self.peek(), Some(b'{' as u16));
        self.pos += 1; // consume '{'
        let min = self.read_decimal();
        let max = if self.eat(b',' as u16) {
            if self.peek() == Some(b'}' as u16) {
                None
            } else {
                Some(self.read_decimal())
            }
        } else {
            Some(min)
        };
        if !self.eat(b'}' as u16) {
            return Err(self.err("unterminated quantifier"));
        }
        Ok((min, max))
    }

    /// Read a legacy octal escape (Annex B B.1.2): 1–3 octal digits, where a
    /// leading digit `0`–`3` permits three digits and `4`–`7` permits two, with
    /// a maximum value of `0o377`. Precondition: the cursor is on an octal digit.
    fn read_legacy_octal(&mut self) -> u32 {
        let first = u32::from(self.units[self.pos] - b'0' as u16);
        self.pos += 1;
        let mut val = first;
        let mut digits = 1;
        let max_digits = if first <= 3 { 3 } else { 2 };
        while digits < max_digits {
            match self.peek() {
                Some(c) if (b'0' as u16..=b'7' as u16).contains(&c) => {
                    val = val * 8 + u32::from(c - b'0' as u16);
                    self.pos += 1;
                    digits += 1;
                }
                _ => break,
            }
        }
        val
    }

    fn read_decimal(&mut self) -> u32 {
        let mut v: u64 = 0;
        while let Some(c) = self.peek() {
            if (b'0' as u16..=b'9' as u16).contains(&c) {
                v = (v * 10 + u64::from(c - b'0' as u16)).min(u64::from(u32::MAX));
                self.pos += 1;
            } else {
                break;
            }
        }
        v.min(u64::from(u32::MAX)) as u32
    }

    // --- Escapes (atom position) --------------------------------------------

    fn parse_escape_atom(&mut self) -> Result<Node, RegexError> {
        debug_assert_eq!(self.peek(), Some(b'\\' as u16));
        self.pos += 1; // consume '\'
        let c = self.peek().ok_or_else(|| self.err("trailing backslash"))?;
        match c {
            x if x == b'b' as u16 => {
                self.pos += 1;
                Ok(Node::Assert(Assertion::WordBoundary {
                    invert: false,
                    ignore_case: self.flags.ignore_case,
                }))
            }
            x if x == b'B' as u16 => {
                self.pos += 1;
                Ok(Node::Assert(Assertion::WordBoundary {
                    invert: true,
                    ignore_case: self.flags.ignore_case,
                }))
            }
            x if x == b'k' as u16 => {
                // `\k<name>` is a named backreference only when the pattern
                // actually contains a named capturing group. With no named
                // group present, Annex B §B.1.4 / §22.2.1 parse `\k` in
                // non-unicode mode as an identity escape (a literal `k`,
                // leaving the following `<…>` as ordinary atoms); unicode
                // mode keeps it a hard error.
                let has_named = !self.group_names.iter().all(Option::is_none);
                if (self.unicode() || has_named) && self.peek_at(1) == Some(b'<' as u16) {
                    let (name, next) = read_group_name(self.units, self.pos + 2, self.unicode())
                        .ok_or_else(|| self.err("invalid backreference name"))?;
                    self.pos = next;
                    let indices = self.name_to_indices(&name);
                    if indices.is_empty() {
                        return Err(self.err("backreference to unknown group name"));
                    }
                    Ok(self.backref_node(indices))
                } else if self.unicode() || has_named {
                    Err(self.err("invalid \\k escape"))
                } else {
                    self.pos += 1;
                    Ok(self.char_node(u32::from(c)))
                }
            }
            x if (b'1' as u16..=b'9' as u16).contains(&x) => self.parse_numeric_backref(),
            // §22.2.1 — in `v` mode an atom-position `\p{...}` / `\P{...}`
            // may name a string property (e.g. `\p{RGI_Emoji}`), which
            // contributes string alternatives, so route it through the
            // set-aware resolver.
            x if (x == b'p' as u16 || x == b'P' as u16) && self.flags.unicode_sets => {
                let negate = x == b'P' as u16;
                self.pos += 1; // consume `p` / `P`
                let set = self.parse_property_set_v(negate)?;
                Ok(self.class_node(set, false))
            }
            _ => {
                if let Some(set) = self.try_class_escape_set()? {
                    let (set, negate) = set;
                    Ok(self.class_node(ClassSet::from_code_points(set), negate))
                } else {
                    let cp = self.parse_char_escape(false)?;
                    Ok(self.char_node(cp))
                }
            }
        }
    }

    fn parse_numeric_backref(&mut self) -> Result<Node, RegexError> {
        let start = self.pos;
        let value = self.read_decimal();
        if value <= self.total_groups {
            Ok(self.backref_node(vec![value]))
        } else if self.unicode() {
            Err(RegexError::Syntax {
                message: "backreference to non-existent group".to_string(),
                offset: start,
            })
        } else {
            // Annex B: an out-of-range decimal escape is a legacy octal escape
            // (`\1`–`\7` lead an octal run) or a NonOctalDecimalEscape (`\8`,
            // `\9` denote the digit literally).
            self.pos = start;
            let c = self.peek().expect("at least one digit");
            if (b'1' as u16..=b'7' as u16).contains(&c) {
                {
                    let cp = self.read_legacy_octal();
                    Ok(self.char_node(cp))
                }
            } else {
                self.pos += 1;
                Ok(self.char_node(u32::from(c)))
            }
        }
    }

    /// A `\`-escape that denotes a class (`\d \D \w \W \s \S`), or `None` if the
    /// next escape is a single-character escape instead.
    fn try_class_escape_set(&mut self) -> Result<Option<(CodePointSet, bool)>, RegexError> {
        let c = self.peek().expect("class escape at end");
        let (base, negate): (fn() -> CodePointSet, bool) = match c {
            x if x == b'd' as u16 => (digit_set, false),
            x if x == b'D' as u16 => (digit_set, true),
            x if x == b'w' as u16 => (word_set, false),
            x if x == b'W' as u16 => (word_set, true),
            x if x == b's' as u16 => (space_set, false),
            x if x == b'S' as u16 => (space_set, true),
            x if (x == b'p' as u16 || x == b'P' as u16) && self.unicode() => {
                // `\p{...}` / `\P{...}` property escapes are only meaningful in
                // unicode mode; in sloppy mode `\p` is an identity escape and is
                // handled by the single-character escape path below.
                let negate = x == b'P' as u16;
                self.pos += 1; // consume `p` / `P`
                let set = self.parse_property_body()?;
                return Ok(Some((set, negate)));
            }
            _ => return Ok(None),
        };
        self.pos += 1;
        Ok(Some((base(), negate)))
    }

    /// Parse the `{name}` / `{name=value}` body of a `\p`/`\P` escape and resolve
    /// it to a code-point set.
    fn parse_property_body(&mut self) -> Result<CodePointSet, RegexError> {
        let (name, value) = self.read_property_name_body()?;
        crate::unicode::resolve_property(&name, value.as_deref())
    }

    /// Parse the `{name}` / `{name=value}` body of a `\p`/`\P` escape into
    /// its name and optional value, leaving the cursor after `}`.
    fn read_property_name_body(&mut self) -> Result<(String, Option<String>), RegexError> {
        if !self.eat(b'{' as u16) {
            return Err(self.err("expected `{` after \\p"));
        }
        let start = self.pos;
        let mut eq: Option<usize> = None;
        while let Some(c) = self.peek() {
            if c == b'}' as u16 {
                break;
            }
            if c == b'=' as u16 && eq.is_none() {
                eq = Some(self.pos);
            }
            self.pos += 1;
        }
        let close = self.pos;
        if !self.eat(b'}' as u16) {
            return Err(self.err("unterminated \\p{...}"));
        }
        let to_str = |units: &[u16]| String::from_utf16_lossy(units);
        Ok(match eq {
            Some(eq) => (
                to_str(&self.units[start..eq]),
                Some(to_str(&self.units[eq + 1..close])),
            ),
            None => (to_str(&self.units[start..close]), None),
        })
    }

    /// `v`-mode `\p{...}` / `\P{...}` operand: a string property (e.g.
    /// `\p{Basic_Emoji}`) resolves to a [`ClassSet`] with string
    /// alternatives; any other property resolves to a code-point set.
    /// A negated string property (`\P{Basic_Emoji}`) is a syntax error.
    fn parse_property_set_v(&mut self, negate: bool) -> Result<ClassSet, RegexError> {
        let (name, value) = self.read_property_name_body()?;
        if value.is_none() && crate::unicode::string_props::is_string_property(&name) {
            if negate {
                return Err(self.err("a negated property may not contain strings"));
            }
            return crate::unicode::string_props::resolve_string_property(&name);
        }
        let set = crate::unicode::resolve_property(&name, value.as_deref())?;
        let set = if negate { set.negate() } else { set };
        Ok(ClassSet::from_code_points(set))
    }

    /// A single-character escape resolving to one code point.
    fn parse_char_escape(&mut self, in_class: bool) -> Result<u32, RegexError> {
        let c = self.peek().expect("char escape at end");
        match c {
            x if x == b'n' as u16 => {
                self.pos += 1;
                Ok(0x0A)
            }
            x if x == b't' as u16 => {
                self.pos += 1;
                Ok(0x09)
            }
            x if x == b'r' as u16 => {
                self.pos += 1;
                Ok(0x0D)
            }
            x if x == b'f' as u16 => {
                self.pos += 1;
                Ok(0x0C)
            }
            x if x == b'v' as u16 => {
                self.pos += 1;
                Ok(0x0B)
            }
            x if x == b'0' as u16 => {
                // `\0` not followed by a digit is NUL. Followed by a digit it
                // is a legacy octal escape (Annex B); in unicode mode that is a
                // syntax error.
                if self
                    .peek_at(1)
                    .is_some_and(|d| (b'0' as u16..=b'9' as u16).contains(&d))
                {
                    if self.unicode() {
                        return Err(self.err("invalid escape: \\0 followed by a digit"));
                    }
                    return Ok(self.read_legacy_octal());
                }
                self.pos += 1;
                Ok(0)
            }
            x if x == b'x' as u16 => {
                self.pos += 1;
                // Annex B §B.1.2: in non-unicode mode `\x` not followed by two
                // hex digits is the identity escape for `x`.
                let two_hex = self.peek().is_some_and(|d| hex_digit(d).is_some())
                    && self.peek_at(1).is_some_and(|d| hex_digit(d).is_some());
                if !self.unicode() && !two_hex {
                    return Ok(u32::from(b'x'));
                }
                self.read_fixed_hex(2)
            }
            x if x == b'u' as u16 => {
                self.pos += 1;
                if self.unicode() {
                    self.read_unicode_escape()
                } else {
                    // Annex B §B.1.2: in non-unicode mode `\u` that is not a
                    // valid `\uXXXX` / `\u{…}` escape is the identity escape
                    // for `u`; restore any hex digits consumed on the failed
                    // attempt so they parse as ordinary atoms.
                    let save = self.pos;
                    match self.read_unicode_escape() {
                        Ok(v) => Ok(v),
                        Err(_) => {
                            self.pos = save;
                            Ok(u32::from(b'u'))
                        }
                    }
                }
            }
            x if x == b'c' as u16 => {
                // `\cX` control escape.
                if let Some(letter) = self.peek_at(1) {
                    let l = letter;
                    if (b'A' as u16..=b'Z' as u16).contains(&l)
                        || (b'a' as u16..=b'z' as u16).contains(&l)
                    {
                        self.pos += 2;
                        return Ok(u32::from(l % 32));
                    }
                }
                if self.unicode() {
                    Err(self.err("invalid \\c escape"))
                } else {
                    // Annex B §B.1.2: `\c` not followed by a ControlLetter is a
                    // literal backslash; the `c` is NOT consumed — it parses as
                    // an ordinary atom so `/\cZ/` (Z non-letter) matches the
                    // three characters `\`, `c`, `Z`.
                    Ok(u32::from(b'\\'))
                }
            }
            _ => {
                // Identity escape. In unicode mode only SyntaxCharacter and `/`
                // may be escaped in an Atom; inside a character class `-` is
                // additionally a valid ClassEscape. Non-unicode mode escapes
                // any character.
                if self.unicode()
                    && !is_syntax_char(c)
                    && c != b'/' as u16
                    && !(in_class && c == b'-' as u16)
                {
                    return Err(self.err("invalid identity escape"));
                }
                let cp = self.read_pattern_codepoint();
                Ok(cp)
            }
        }
    }

    fn read_fixed_hex(&mut self, n: usize) -> Result<u32, RegexError> {
        let mut v = 0u32;
        for _ in 0..n {
            let c = self
                .peek()
                .ok_or_else(|| self.err("incomplete hex escape"))?;
            let d = hex_digit(c).ok_or_else(|| self.err("invalid hex digit"))?;
            v = v * 16 + d;
            self.pos += 1;
        }
        Ok(v)
    }

    fn read_unicode_escape(&mut self) -> Result<u32, RegexError> {
        if self.eat(b'{' as u16) {
            // `\u{...}` — only valid in unicode mode.
            if !self.unicode() {
                return Err(self.err("\\u{...} requires the u flag"));
            }
            let mut v = 0u32;
            let mut any = false;
            while let Some(c) = self.peek() {
                if c == b'}' as u16 {
                    break;
                }
                let d = hex_digit(c).ok_or_else(|| self.err("invalid hex digit"))?;
                v = v.saturating_mul(16) + d;
                if v > 0x10FFFF {
                    return Err(self.err("code point out of range"));
                }
                any = true;
                self.pos += 1;
            }
            if !any || !self.eat(b'}' as u16) {
                return Err(self.err("malformed \\u{...} escape"));
            }
            Ok(v)
        } else {
            let hi = self.read_fixed_hex(4)?;
            // In unicode mode, combine a `\uXXXX\uXXXX` surrogate pair.
            if self.unicode() && (0xD800..=0xDBFF).contains(&hi) {
                let save = self.pos;
                if self.eat(b'\\' as u16)
                    && self.eat(b'u' as u16)
                    && let Ok(lo) = self.read_fixed_hex(4)
                    && (0xDC00..=0xDFFF).contains(&lo)
                {
                    return Ok(0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00));
                }
                self.pos = save;
            }
            Ok(hi)
        }
    }

    // --- Character classes ---------------------------------------------------

    fn parse_class(&mut self) -> Result<Node, RegexError> {
        // §22.2.1.4 — under the `v` flag a class is a `ClassSetExpression`
        // (nested classes, `--`/`&&` set operations, `\q{...}` string
        // alternatives) rather than the legacy character-class grammar.
        if self.flags.unicode_sets {
            let (set, negate) = self.parse_class_set()?;
            return Ok(self.class_node(set, negate));
        }
        self.enter()?;
        debug_assert_eq!(self.peek(), Some(b'[' as u16));
        self.pos += 1; // consume '['
        let negate = self.eat(b'^' as u16);
        let mut set = CodePointSet::new();
        loop {
            match self.peek() {
                None => {
                    self.leave();
                    return Err(self.err("unterminated character class"));
                }
                Some(c) if c == b']' as u16 => {
                    self.pos += 1;
                    break;
                }
                _ => self.parse_class_member(&mut set)?,
            }
        }
        self.leave();
        Ok(self.class_node(ClassSet::from_code_points(set), negate))
    }

    /// §22.2.1.4 `ClassSetExpression` — parse a `v`-mode `[...]` (already
    /// positioned at `[`) into a resolved [`ClassSet`] plus the negation
    /// flag. A negated set that may contain strings is a syntax error.
    fn parse_class_set(&mut self) -> Result<(ClassSet, bool), RegexError> {
        self.enter()?;
        debug_assert_eq!(self.peek(), Some(b'[' as u16));
        self.pos += 1; // consume '['
        let negate = self.eat(b'^' as u16);
        let mut set = ClassSet::default();
        // Empty class `[]` / `[^]`.
        if self.peek() == Some(b']' as u16) {
            self.pos += 1;
            self.leave();
            return self.finish_class_set(set, negate);
        }
        // First member fixes whether this is a union, intersection, or
        // difference; the three operators do not mix in one class.
        self.parse_class_set_union_member(&mut set)?;
        match (self.peek(), self.peek_at(1)) {
            (Some(a), Some(b)) if a == b'&' as u16 && b == b'&' as u16 => {
                while self.peek() == Some(b'&' as u16) && self.peek_at(1) == Some(b'&' as u16) {
                    self.pos += 2;
                    let operand = self.parse_class_set_operand()?;
                    set = set.intersection(&operand);
                }
            }
            (Some(a), Some(b)) if a == b'-' as u16 && b == b'-' as u16 => {
                while self.peek() == Some(b'-' as u16) && self.peek_at(1) == Some(b'-' as u16) {
                    self.pos += 2;
                    let operand = self.parse_class_set_operand()?;
                    set = set.difference(&operand);
                }
            }
            _ => {
                while self.peek().is_some_and(|c| c != b']' as u16) {
                    self.parse_class_set_union_member(&mut set)?;
                }
            }
        }
        if !self.eat(b']' as u16) {
            self.leave();
            return Err(self.err("unterminated character class"));
        }
        self.leave();
        self.finish_class_set(set, negate)
    }

    /// Apply the `[^...]` negation, rejecting it when the set may contain
    /// strings (§22.2.1.4 `MayContainStrings`).
    fn finish_class_set(
        &self,
        set: ClassSet,
        negate: bool,
    ) -> Result<(ClassSet, bool), RegexError> {
        if negate {
            if set.may_contain_strings() {
                return Err(RegexError::Syntax {
                    message: "negated character class may contain strings".to_string(),
                    offset: self.pos,
                });
            }
            return Ok((set.negate_code_points(), true));
        }
        Ok((set, false))
    }

    /// One `ClassUnion` member: a `ClassSetRange` (`a-z`) or a
    /// `ClassSetOperand`, folded into `acc`.
    fn parse_class_set_union_member(&mut self, acc: &mut ClassSet) -> Result<(), RegexError> {
        // A bare `ClassSetCharacter` can begin a range; a set-valued
        // operand (nested class, `\p`, `\d`, `\q`) cannot.
        if let Some(lo) = self.try_class_set_character()? {
            // A single `-` forms a range; a `--` is the set-difference
            // operator (handled by the caller), not a range dash.
            if self.peek() == Some(b'-' as u16)
                && self.peek_at(1) != Some(b'-' as u16)
                && self.peek_at(1) != Some(b']' as u16)
            {
                self.pos += 1; // consume '-'
                let hi = self.try_class_set_character()?.ok_or_else(|| {
                    self.err("character class range needs a character upper bound")
                })?;
                if lo > hi {
                    return Err(self.err("character class range out of order"));
                }
                acc.code_points.insert_range(lo, hi);
            } else {
                acc.code_points.insert(lo);
            }
            return Ok(());
        }
        let operand = self.parse_class_set_operand()?;
        acc.union_with(&operand);
        Ok(())
    }

    /// A `ClassSetOperand` that is *not* a bare character: a nested class,
    /// a `\q{...}` string disjunction, or a class escape (`\p`, `\d`, …).
    fn parse_class_set_operand(&mut self) -> Result<ClassSet, RegexError> {
        match self.peek() {
            Some(c) if c == b'[' as u16 => {
                let (set, _negate) = self.parse_class_set()?;
                Ok(set)
            }
            Some(c) if c == b'\\' as u16 => {
                if self.peek_at(1) == Some(b'q' as u16) {
                    self.pos += 2; // consume `\q`
                    return self.parse_class_string_disjunction();
                }
                // `\p{...}` / `\P{...}` may name a string property in `v`
                // mode, so route it through the set-aware resolver.
                if matches!(self.peek_at(1), Some(x) if x == b'p' as u16 || x == b'P' as u16)
                    && self.unicode()
                {
                    let negate = self.peek_at(1) == Some(b'P' as u16);
                    self.pos += 2; // consume `\p` / `\P`
                    return self.parse_property_set_v(negate);
                }
                self.pos += 1; // consume '\'
                if let Some((sub, negate)) = self.try_class_escape_set()? {
                    let sub = if negate { sub.negate() } else { sub };
                    return Ok(ClassSet::from_code_points(sub));
                }
                let cp = self.parse_class_char_escape()?;
                let mut set = CodePointSet::new();
                set.insert(cp);
                Ok(ClassSet::from_code_points(set))
            }
            _ => {
                // A lone character reached here (e.g. as an `&&`/`--`
                // operand) is a single-code-point operand.
                let cp = self
                    .try_class_set_character()?
                    .ok_or_else(|| self.err("expected character class operand"))?;
                let mut set = CodePointSet::new();
                set.insert(cp);
                Ok(ClassSet::from_code_points(set))
            }
        }
    }

    /// `\q{ Alt | Alt | ... }` — string alternatives. Each alternative is
    /// a (possibly empty) sequence of `ClassSetCharacter`s; a
    /// single-character alternative is folded into the code-point set.
    fn parse_class_string_disjunction(&mut self) -> Result<ClassSet, RegexError> {
        if !self.eat(b'{' as u16) {
            return Err(self.err("expected `{` after \\q"));
        }
        let mut set = ClassSet::default();
        loop {
            let mut alt: Vec<u32> = Vec::new();
            while let Some(c) = self.peek() {
                if c == b'|' as u16 || c == b'}' as u16 {
                    break;
                }
                let cp = self
                    .try_class_set_character()?
                    .ok_or_else(|| self.err("invalid \\q{...} alternative"))?;
                alt.push(cp);
            }
            set.add_alternative(alt);
            if self.eat(b'}' as u16) {
                break;
            }
            if !self.eat(b'|' as u16) {
                return Err(self.err("unterminated \\q{...}"));
            }
        }
        Ok(set)
    }

    /// A single `ClassSetCharacter` (`v`-mode): a literal code point or a
    /// character escape, returning `None` when the cursor is on a
    /// set-valued construct (`[`, `\p`, `\d`, `\q`) or a class delimiter.
    fn try_class_set_character(&mut self) -> Result<Option<u32>, RegexError> {
        match self.peek() {
            None => Ok(None),
            Some(c) if c == b']' as u16 || c == b'[' as u16 => Ok(None),
            Some(c) if c == b'\\' as u16 => {
                // A class-escape set or `\q` is an operand, not a character.
                let next = self.peek_at(1);
                let is_set_escape = matches!(
                    next,
                    Some(x) if x == b'd' as u16 || x == b'D' as u16
                        || x == b'w' as u16 || x == b'W' as u16
                        || x == b's' as u16 || x == b'S' as u16
                        || x == b'q' as u16
                        || ((x == b'p' as u16 || x == b'P' as u16) && self.unicode())
                );
                if is_set_escape {
                    return Ok(None);
                }
                self.pos += 1; // consume '\'
                Ok(Some(self.parse_class_char_escape()?))
            }
            Some(_) => Ok(Some(self.read_pattern_codepoint())),
        }
    }

    fn parse_class_member(&mut self, set: &mut CodePointSet) -> Result<(), RegexError> {
        // A class-escape (`\d` etc.) contributes a whole sub-set and cannot be a
        // range endpoint.
        if self.peek() == Some(b'\\' as u16) {
            self.pos += 1;
            if let Some((sub, negate)) = self.try_class_escape_set()? {
                let sub = if negate { sub.negate() } else { sub };
                set.union_with(&sub);
                // §22.2.1.4 — a CharacterClassEscape (`\d`, `\p{…}`, …) cannot
                // be a range endpoint. In unicode mode `[\d-a]` is a syntax
                // error; Annex B keeps the `-` literal (handled by the union
                // loop's next iteration).
                if self.unicode()
                    && self.peek() == Some(b'-' as u16)
                    && self.peek_at(1) != Some(b']' as u16)
                {
                    return Err(self.err("invalid class range"));
                }
                return Ok(());
            }
            let lo = self.parse_class_char_escape()?;
            return self.finish_class_atom(set, lo);
        }
        let lo = self.read_pattern_codepoint();
        self.finish_class_atom(set, lo)
    }

    /// Having read a single code point `lo`, handle an optional `-hi` range.
    fn finish_class_atom(&mut self, set: &mut CodePointSet, lo: u32) -> Result<(), RegexError> {
        // A `-` forms a range unless it is the last character before `]`.
        if self.peek() == Some(b'-' as u16) && self.peek_at(1) != Some(b']' as u16) {
            self.pos += 1; // consume '-'
            // The high endpoint must be a single character, not a class escape.
            if self.peek() == Some(b'\\' as u16) {
                self.pos += 1;
                if self.next_is_class_escape() {
                    // `a-\d` — in unicode mode this is an error; otherwise treat
                    // `-` as a literal and add the escape set separately.
                    if self.unicode() {
                        return Err(self.err("invalid class range"));
                    }
                    set.insert(lo);
                    set.insert(u32::from(b'-'));
                    let (sub, negate) = self
                        .try_class_escape_set()?
                        .expect("checked next_is_class_escape");
                    let sub = if negate { sub.negate() } else { sub };
                    set.union_with(&sub);
                    return Ok(());
                }
                let hi = self.parse_class_char_escape()?;
                return self.insert_class_range(set, lo, hi);
            }
            let hi = self.read_pattern_codepoint();
            return self.insert_class_range(set, lo, hi);
        }
        set.insert(lo);
        Ok(())
    }

    fn insert_class_range(
        &self,
        set: &mut CodePointSet,
        lo: u32,
        hi: u32,
    ) -> Result<(), RegexError> {
        if lo > hi {
            return Err(RegexError::Syntax {
                message: "character class range out of order".to_string(),
                offset: self.pos,
            });
        }
        set.insert_range(lo, hi);
        Ok(())
    }

    fn next_is_class_escape(&self) -> bool {
        matches!(
            self.peek(),
            Some(c) if c == b'd' as u16
                || c == b'D' as u16
                || c == b'w' as u16
                || c == b'W' as u16
                || c == b's' as u16
                || c == b'S' as u16
        )
    }

    /// A single-character escape inside a class. Differs from the atom-position
    /// version only in that `\b` is a backspace, not a word boundary.
    fn parse_class_char_escape(&mut self) -> Result<u32, RegexError> {
        if self.peek() == Some(b'b' as u16) {
            self.pos += 1;
            return Ok(0x08);
        }
        // Legacy octal escape inside a class (Annex B, non-unicode): `\1`–`\7`.
        // `\0` is handled by the shared char-escape path.
        if !self.unicode()
            && let Some(c) = self.peek()
            && (b'1' as u16..=b'7' as u16).contains(&c)
        {
            return Ok(self.read_legacy_octal());
        }
        self.parse_char_escape(true)
    }
}

// --- Free helpers ------------------------------------------------------------

/// Read a `(?<name>` / `\k<name>` group name starting at `start` (just past the
/// `<`). Returns the name and the index just past the closing `>`.
/// `true` when `node` is a zero-width assertion (`^ $ \b \B`) or a
/// lookaround group — the atoms a Quantifier may not follow in unicode mode.
fn is_assertion_node(node: &Node) -> bool {
    matches!(
        node,
        Node::Assert(_)
            | Node::Group {
                kind: GroupKind::Lookahead { .. } | GroupKind::Lookbehind { .. },
                ..
            }
    )
}

fn read_group_name(units: &[u16], start: usize, unicode: bool) -> Option<(String, usize)> {
    let mut i = start;
    let mut name = String::new();
    let mut first = true;
    while let Some(&c) = units.get(i) {
        if c == b'>' as u16 {
            if name.is_empty() {
                return None;
            }
            return Some((name, i + 1));
        }
        let (ch, next) = if c == b'\\' as u16 && units.get(i + 1) == Some(&(b'u' as u16)) {
            read_group_name_unicode_escape(units, i + 2, unicode)?
        } else {
            read_group_name_code_point(units, i)?
        };
        if first {
            if !is_group_name_start(ch) {
                return None;
            }
            first = false;
        } else if !is_group_name_continue(ch) {
            return None;
        }
        name.push(ch);
        i = next;
    }
    None
}

fn read_group_name_unicode_escape(
    units: &[u16],
    start: usize,
    _unicode: bool,
) -> Option<(char, usize)> {
    if units.get(start) == Some(&(b'{' as u16)) {
        let mut i = start + 1;
        let mut value = 0u32;
        let mut any = false;
        while let Some(&c) = units.get(i) {
            if c == b'}' as u16 {
                if !any {
                    return None;
                }
                return char::from_u32(value).map(|ch| (ch, i + 1));
            }
            let digit = hex_digit(c)?;
            value = value.checked_mul(16)?.checked_add(digit)?;
            if value > 0x10FFFF {
                return None;
            }
            any = true;
            i += 1;
        }
        return None;
    }

    let (hi, mut next) = read_group_name_fixed_hex(units, start)?;
    if (0xD800..=0xDBFF).contains(&hi)
        && units.get(next) == Some(&(b'\\' as u16))
        && units.get(next + 1) == Some(&(b'u' as u16))
        && let Some((lo, after)) = read_group_name_fixed_hex(units, next + 2)
        && (0xDC00..=0xDFFF).contains(&lo)
    {
        next = after;
        let cp = 0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00);
        return char::from_u32(cp).map(|ch| (ch, next));
    }
    char::from_u32(hi).map(|ch| (ch, next))
}

fn read_group_name_fixed_hex(units: &[u16], start: usize) -> Option<(u32, usize)> {
    let mut value = 0u32;
    let mut i = start;
    for _ in 0..4 {
        value = value * 16 + hex_digit(*units.get(i)?)?;
        i += 1;
    }
    Some((value, i))
}

fn read_group_name_code_point(units: &[u16], start: usize) -> Option<(char, usize)> {
    let hi = *units.get(start)?;
    if (0xD800..=0xDBFF).contains(&hi)
        && let Some(&lo) = units.get(start + 1)
        && (0xDC00..=0xDFFF).contains(&lo)
    {
        let cp = 0x10000 + ((u32::from(hi) - 0xD800) << 10) + (u32::from(lo) - 0xDC00);
        return char::from_u32(cp).map(|ch| (ch, start + 2));
    }
    char::from_u32(u32::from(hi)).map(|ch| (ch, start + 1))
}

fn is_group_name_start(ch: char) -> bool {
    ch == '$' || ch == '_' || unicode_ident::is_xid_start(ch)
}

fn is_group_name_continue(ch: char) -> bool {
    is_group_name_start(ch)
        || unicode_ident::is_xid_continue(ch)
        || matches!(ch, '\u{200c}' | '\u{200d}')
}

fn hex_digit(c: u16) -> Option<u32> {
    match c {
        x if (b'0' as u16..=b'9' as u16).contains(&x) => Some(u32::from(x - b'0' as u16)),
        x if (b'a' as u16..=b'f' as u16).contains(&x) => Some(u32::from(x - b'a' as u16) + 10),
        x if (b'A' as u16..=b'F' as u16).contains(&x) => Some(u32::from(x - b'A' as u16) + 10),
        _ => None,
    }
}

fn is_syntax_char(c: u16) -> bool {
    matches!(
        c as u8 as char,
        '^' | '$' | '\\' | '.' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|'
    ) && c < 0x80
}

fn digit_set() -> CodePointSet {
    let mut s = CodePointSet::new();
    s.insert_range(u32::from(b'0'), u32::from(b'9'));
    s
}

fn word_set() -> CodePointSet {
    let mut s = CodePointSet::new();
    s.insert_range(u32::from(b'0'), u32::from(b'9'));
    s.insert_range(u32::from(b'A'), u32::from(b'Z'));
    s.insert_range(u32::from(b'a'), u32::from(b'z'));
    s.insert(u32::from(b'_'));
    s
}

fn space_set() -> CodePointSet {
    // WhiteSpace ∪ LineTerminator (§22.2.2.7 \s).
    let mut s = CodePointSet::new();
    for cp in [
        0x09, 0x0A, 0x0B, 0x0C, 0x0D, 0x20, 0xA0, 0x1680, 0x2028, 0x2029, 0x202F, 0x205F, 0x3000,
        0xFEFF,
    ] {
        s.insert(cp);
    }
    s.insert_range(0x2000, 0x200A);
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_ok(pat: &str, flags: Flags) -> Parsed {
        let units: Vec<u16> = pat.encode_utf16().collect();
        parse(&units, flags).expect("parse should succeed")
    }

    fn parse_err(pat: &str, flags: Flags) -> RegexError {
        let units: Vec<u16> = pat.encode_utf16().collect();
        parse(&units, flags).expect_err("parse should fail")
    }

    #[test]
    fn counts_capturing_groups() {
        let p = parse_ok("(a)(?:b)(c)", Flags::default());
        assert_eq!(p.group_count, 2);
    }

    #[test]
    fn records_named_groups_in_order() {
        let p = parse_ok("(?<first>a)(?<second>b)", Flags::default());
        assert_eq!(
            p.group_names,
            vec![Some("first".to_string()), Some("second".to_string())]
        );
    }

    #[test]
    fn rejects_duplicate_named_groups_in_same_alternative() {
        assert!(matches!(
            parse_err("(?<x>a)(?<x>b)", Flags::default()),
            RegexError::Syntax { .. }
        ));
        let _ = parse_ok("(?<x>a)|(?<x>b)", Flags::default());
        assert!(matches!(
            parse_err("(?<x>a)(?:b|(?<x>c))", Flags::default()),
            RegexError::Syntax { .. }
        ));
    }

    #[test]
    fn decodes_escaped_named_group_names() {
        let u = Flags {
            unicode: true,
            ..Flags::default()
        };
        let p = parse_ok("(?<\\u{03C0}>a)(?<a\\u{104A4}>b)", u);
        assert_eq!(
            p.group_names,
            vec![Some("π".to_string()), Some("a𐒤".to_string())]
        );

        let p = parse_ok(
            "(?<\\u{1d4d1}\\u{1d4fb}\\u{1d4f8}\\u{1d500}\\u{1d4f7}>brown)",
            Flags::default(),
        );
        assert_eq!(p.group_names, vec![Some("𝓑𝓻𝓸𝔀𝓷".to_string())]);

        let p = parse_ok(
            "(?<\\ud835\\udcd1\\ud835\\udcfb\\ud835\\udcf8\\ud835\\udd00\\ud835\\udcf7>brown)",
            Flags::default(),
        );
        assert_eq!(p.group_names, vec![Some("𝓑𝓻𝓸𝔀𝓷".to_string())]);
    }

    #[test]
    fn validates_named_group_identifier_names() {
        let u = Flags {
            unicode: true,
            ..Flags::default()
        };
        let _ = parse_ok("(?<$>a)(?<_\\u200C>b)(?<ಠ_ಠ>c)", u);
        assert!(matches!(
            parse_err("(?<🦊>fox)", u),
            RegexError::Syntax { .. }
        ));
        assert!(matches!(
            parse_err("(?<𝟚the>the)", u),
            RegexError::Syntax { .. }
        ));
    }

    #[test]
    fn forward_named_backref_resolves() {
        // Reference appears before the group is defined.
        let _ = parse_ok("\\k<x>(?<x>a)", Flags::default());
    }

    #[test]
    fn nesting_limit_is_enforced() {
        let deep = "(".repeat(MAX_NESTING_DEPTH + 5);
        let err = parse_err(&deep, Flags::default());
        assert!(matches!(err, RegexError::TooDeep { .. }));
    }

    #[test]
    fn property_escapes_parse_in_unicode_mode() {
        let u = Flags {
            unicode: true,
            ..Flags::default()
        };
        let _ = parse_ok("\\p{Letter}", u);
        let _ = parse_ok("\\p{Script=Greek}", u);
        let _ = parse_ok("\\P{Nd}", u);
        // Unknown property is a syntax error, not a silent empty set.
        assert!(matches!(
            parse_err("\\p{NotAProperty}", u),
            RegexError::Syntax { .. }
        ));
    }

    #[test]
    fn unmatched_paren_errors() {
        let err = parse_err("a)", Flags::default());
        assert!(matches!(err, RegexError::Syntax { .. }));
    }
}
