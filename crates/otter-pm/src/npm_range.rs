//! npm's version range grammar, as registry packages write their
//! dependency ranges.
//!
//! A range is one or more comparator sets joined by `||`; a version
//! satisfies the range when it satisfies every comparator of some set. Sets
//! are written with npm's sugar — hyphen ranges, X-ranges and partial
//! versions, tilde and caret ranges — which desugars into plain comparators
//! the way node-semver's `Range` does.
//!
//! # Contents
//! - [`NpmRange`] — a parsed range: [`NpmRange::parse`] and
//!   [`NpmRange::matches`].
//!
//! # Invariants
//! - A prerelease version satisfies a set only when some comparator of that
//!   set names a prerelease of the same `major.minor.patch`: `^1.2.3` admits
//!   no `1.5.0-beta`, and `>=1.2.3-alpha.1` admits `1.2.3-alpha.7` but not
//!   `1.2.4-alpha.1`.
//! - An upper bound desugared from a partial version excludes that
//!   version's prereleases: `^1.2.3` is `>=1.2.3 <2.0.0-0`.
//! - Build metadata takes no part in matching.
//! - A set that does not parse admits nothing; a range none of whose sets
//!   parses is not a range.
//!
//! # See also
//! - node-semver's `Range`, the grammar followed here.

use semver::{BuildMetadata, Prerelease, Version};

/// A parsed npm version range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NpmRange {
    sets: Vec<Vec<Comparator>>,
}

impl NpmRange {
    /// Parse `range`; `None` when none of its sets is npm range syntax. A
    /// set that is not (a dist-tag written among versions, say) admits
    /// nothing and leaves the others in force.
    #[must_use]
    pub fn parse(range: &str) -> Option<Self> {
        let sets = range
            .split("||")
            .filter_map(|set| parse_set(set.trim()))
            .collect::<Vec<_>>();
        (!sets.is_empty()).then_some(Self { sets })
    }

    /// Whether `version` satisfies the range.
    #[must_use]
    pub fn matches(&self, version: &Version) -> bool {
        let version = Version {
            build: BuildMetadata::EMPTY,
            ..version.clone()
        };
        self.sets.iter().any(|set| set_matches(set, &version))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Eq,
    Gt,
    Ge,
    Lt,
    Le,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Comparator {
    op: Op,
    version: Version,
}

impl Comparator {
    fn matches(&self, version: &Version) -> bool {
        match self.op {
            Op::Eq => version == &self.version,
            Op::Gt => version > &self.version,
            Op::Ge => version >= &self.version,
            Op::Lt => version < &self.version,
            Op::Le => version <= &self.version,
        }
    }
}

fn set_matches(set: &[Comparator], version: &Version) -> bool {
    if !set.iter().all(|comparator| comparator.matches(version)) {
        return false;
    }
    if version.pre.is_empty() {
        return true;
    }
    set.iter().any(|comparator| {
        !comparator.version.pre.is_empty()
            && comparator.version.major == version.major
            && comparator.version.minor == version.minor
            && comparator.version.patch == version.patch
    })
}

/// One possibly partial version: `None` components were omitted or written
/// as `x`, `X` or `*`.
#[derive(Debug, Clone)]
struct Partial {
    major: Option<u64>,
    minor: Option<u64>,
    patch: Option<u64>,
    pre: Prerelease,
}

impl Partial {
    fn parse(text: &str) -> Option<Self> {
        let text = text.trim().trim_start_matches(['v', 'V', '=']).trim();
        // Build metadata never takes part in matching.
        let text = text.split_once('+').map_or(text, |(version, _)| version);
        let (core, pre) = match text.split_once('-') {
            Some((core, pre)) => (core, Prerelease::new(pre).ok()?),
            // npm also reads a prerelease written straight after the patch
            // number: `1.2.3beta`.
            None => match text.find(|c: char| c.is_ascii_alphabetic() && !matches!(c, 'x' | 'X')) {
                Some(at) if at > 0 && text.as_bytes()[at - 1].is_ascii_digit() => {
                    (&text[..at], Prerelease::new(&text[at..]).ok()?)
                }
                _ => (text, Prerelease::EMPTY),
            },
        };
        let mut parts = core.split('.');
        let component = |part: Option<&str>| -> Option<Option<u64>> {
            match part {
                None | Some("x" | "X" | "*") => Some(None),
                Some(digits)
                    if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) =>
                {
                    digits.parse().ok().map(Some)
                }
                Some(_) => None,
            }
        };
        let major = component(parts.next().filter(|part| !part.is_empty()))?;
        let minor = component(parts.next())?;
        let patch = component(parts.next())?;
        if parts.next().is_some() {
            return None;
        }
        // A wildcard component makes every later one a wildcard.
        let minor = major.and(minor);
        let patch = minor.and(patch);
        Some(Self {
            major,
            minor,
            patch,
            pre,
        })
    }
}

fn version(major: u64, minor: u64, patch: u64, pre: Prerelease) -> Version {
    Version {
        major,
        minor,
        patch,
        pre,
        build: BuildMetadata::EMPTY,
    }
}

/// `major.minor.patch-0`: below every prerelease of that version, so an
/// exclusive upper bound at it admits none of them.
fn floor(major: u64, minor: u64, patch: u64) -> Version {
    version(
        major,
        minor,
        patch,
        Prerelease::new("0").expect("`0` is a prerelease"),
    )
}

fn comparator(op: Op, version: Version) -> Comparator {
    Comparator { op, version }
}

fn parse_set(set: &str) -> Option<Vec<Comparator>> {
    if let Some((from, to)) = split_hyphen(set) {
        return hyphen(Partial::parse(from)?, Partial::parse(to)?);
    }
    let mut comparators = Vec::new();
    let mut tokens = set.split_whitespace().peekable();
    while let Some(token) = tokens.next() {
        // An operator may stand apart from its version: `>= 1.2.3`.
        let joined;
        let token = if matches!(token, ">" | ">=" | "<" | "<=" | "=" | "~" | "~>" | "^") {
            joined = format!("{token}{}", tokens.next()?);
            joined.as_str()
        } else {
            token
        };
        comparators.extend(desugar(token)?);
    }
    Some(comparators)
}

/// `a - b`, the hyphen standing alone between two versions.
fn split_hyphen(set: &str) -> Option<(&str, &str)> {
    let (from, to) = set.split_once(" - ")?;
    Some((from.trim(), to.trim()))
}

fn hyphen(from: Partial, to: Partial) -> Option<Vec<Comparator>> {
    let mut set = Vec::new();
    if let Some(major) = from.major {
        let lower = match (from.minor, from.patch) {
            (None, _) => version(major, 0, 0, Prerelease::EMPTY),
            (Some(minor), None) => version(major, minor, 0, Prerelease::EMPTY),
            (Some(minor), Some(patch)) => version(major, minor, patch, from.pre),
        };
        set.push(comparator(Op::Ge, lower));
    }
    if let Some(major) = to.major {
        set.push(match (to.minor, to.patch) {
            (None, _) => comparator(Op::Lt, floor(major.checked_add(1)?, 0, 0)),
            (Some(minor), None) => comparator(Op::Lt, floor(major, minor.checked_add(1)?, 0)),
            (Some(minor), Some(patch)) => comparator(Op::Le, version(major, minor, patch, to.pre)),
        });
    }
    Some(set)
}

fn desugar(token: &str) -> Option<Vec<Comparator>> {
    if let Some(rest) = token.strip_prefix("~>").or_else(|| token.strip_prefix('~')) {
        return tilde(Partial::parse(rest)?);
    }
    if let Some(rest) = token.strip_prefix('^') {
        return caret(Partial::parse(rest)?);
    }
    let (op, rest) = [
        (">=", Op::Ge),
        ("<=", Op::Le),
        (">", Op::Gt),
        ("<", Op::Lt),
        ("=", Op::Eq),
    ]
    .into_iter()
    .find_map(|(prefix, op)| token.strip_prefix(prefix).map(|rest| (op, rest)))
    .unwrap_or((Op::Eq, token));
    x_range(op, Partial::parse(rest)?)
}

fn tilde(partial: Partial) -> Option<Vec<Comparator>> {
    let Some(major) = partial.major else {
        return Some(Vec::new());
    };
    let Some(minor) = partial.minor else {
        return Some(vec![
            comparator(Op::Ge, version(major, 0, 0, Prerelease::EMPTY)),
            comparator(Op::Lt, floor(major.checked_add(1)?, 0, 0)),
        ]);
    };
    let patch = partial.patch.unwrap_or(0);
    let pre = if partial.patch.is_some() {
        partial.pre
    } else {
        Prerelease::EMPTY
    };
    Some(vec![
        comparator(Op::Ge, version(major, minor, patch, pre)),
        comparator(Op::Lt, floor(major, minor.checked_add(1)?, 0)),
    ])
}

fn caret(partial: Partial) -> Option<Vec<Comparator>> {
    let Some(major) = partial.major else {
        return Some(Vec::new());
    };
    let Some(minor) = partial.minor else {
        return Some(vec![
            comparator(Op::Ge, version(major, 0, 0, Prerelease::EMPTY)),
            comparator(Op::Lt, floor(major.checked_add(1)?, 0, 0)),
        ]);
    };
    let Some(patch) = partial.patch else {
        let upper = if major == 0 {
            floor(0, minor.checked_add(1)?, 0)
        } else {
            floor(major.checked_add(1)?, 0, 0)
        };
        return Some(vec![
            comparator(Op::Ge, version(major, minor, 0, Prerelease::EMPTY)),
            comparator(Op::Lt, upper),
        ]);
    };
    // The leftmost nonzero component may not change.
    let upper = match (major, minor) {
        (0, 0) => floor(0, 0, patch.checked_add(1)?),
        (0, _) => floor(0, minor.checked_add(1)?, 0),
        _ => floor(major.checked_add(1)?, 0, 0),
    };
    Some(vec![
        comparator(Op::Ge, version(major, minor, patch, partial.pre)),
        comparator(Op::Lt, upper),
    ])
}

fn x_range(op: Op, partial: Partial) -> Option<Vec<Comparator>> {
    let Some(major) = partial.major else {
        // `*` and its kin: everything, unless the operator excludes it all.
        return Some(if matches!(op, Op::Gt | Op::Lt) {
            vec![comparator(Op::Lt, floor(0, 0, 0))]
        } else {
            Vec::new()
        });
    };
    let (Some(minor), Some(patch)) = (partial.minor, partial.patch) else {
        let minor = partial.minor;
        return Some(match op {
            Op::Eq => match minor {
                None => vec![
                    comparator(Op::Ge, version(major, 0, 0, Prerelease::EMPTY)),
                    comparator(Op::Lt, floor(major.checked_add(1)?, 0, 0)),
                ],
                Some(minor) => vec![
                    comparator(Op::Ge, version(major, minor, 0, Prerelease::EMPTY)),
                    comparator(Op::Lt, floor(major, minor.checked_add(1)?, 0)),
                ],
            },
            // Past every version the partial one covers.
            Op::Gt => vec![match minor {
                None => comparator(
                    Op::Ge,
                    version(major.checked_add(1)?, 0, 0, Prerelease::EMPTY),
                ),
                Some(minor) => comparator(
                    Op::Ge,
                    version(major, minor.checked_add(1)?, 0, Prerelease::EMPTY),
                ),
            }],
            Op::Ge => vec![comparator(
                Op::Ge,
                version(major, minor.unwrap_or(0), 0, Prerelease::EMPTY),
            )],
            Op::Lt => vec![comparator(Op::Lt, floor(major, minor.unwrap_or(0), 0))],
            // Up to the end of what the partial version covers.
            Op::Le => vec![match minor {
                None => comparator(Op::Lt, floor(major.checked_add(1)?, 0, 0)),
                Some(minor) => comparator(Op::Lt, floor(major, minor.checked_add(1)?, 0)),
            }],
        });
    };
    Some(vec![comparator(
        op,
        version(major, minor, patch, partial.pre),
    )])
}

#[cfg(test)]
mod tests {
    use super::NpmRange;

    fn satisfies(range: &str, version: &str) -> bool {
        NpmRange::parse(range)
            .unwrap_or_else(|| panic!("`{range}` parses"))
            .matches(&semver::Version::parse(version).unwrap())
    }

    #[test]
    fn ranges_match_as_npm_matches_them() {
        let yes = [
            ("^3.0.0 || ^4.0.0", "4.0.0"),
            ("^3.0.0 || ^4.0.0", "3.9.1"),
            ("1.2.3 - 2.3.4", "2.3.4"),
            ("1.2 - 2.3", "2.3.9"),
            ("1.x", "1.9.9"),
            ("1.2.*", "1.2.7"),
            ("*", "0.0.1"),
            ("", "3.0.0"),
            ("~1.2", "1.2.9"),
            ("~1", "1.7.0"),
            ("~0.2.3", "0.2.9"),
            ("^0.2.3", "0.2.9"),
            ("^0.0.3", "0.0.3"),
            ("^1.2.x", "1.9.0"),
            ("^0.0.x", "0.0.9"),
            ("^0.x", "0.9.0"),
            (">=1.2.7 <1.3.0", "1.2.8"),
            (">= 1.2.7 < 1.3.0", "1.2.8"),
            (">1.2", "1.3.0"),
            ("<=1.2", "1.2.9"),
            ("<1.2", "1.1.9"),
            ("=1.2.3", "1.2.3"),
            ("v1.2.3", "1.2.3"),
            (">=1.2.3-alpha.1", "1.2.3-alpha.7"),
            ("^1.2.3-beta.2", "1.2.3-beta.4"),
            ("~1.2.3-beta.2", "1.2.3-beta.4"),
            ("1.2.3+build", "1.2.3"),
        ];
        for (range, version) in yes {
            assert!(satisfies(range, version), "`{range}` admits {version}");
        }
        let no = [
            ("^3.0.0 || ^4.0.0", "5.0.0"),
            ("1.2.3 - 2.3.4", "2.3.5"),
            ("1.2 - 2.3", "2.4.0"),
            ("1.x", "2.0.0"),
            ("~1.2", "1.3.0"),
            ("^0.2.3", "0.3.0"),
            ("^0.0.3", "0.0.4"),
            ("^1.2.3", "2.0.0-beta.1"),
            ("^1.2.3", "1.5.0-beta"),
            (">=1.2.3-alpha.1", "1.2.4-alpha.1"),
            ("*", "1.0.0-rc.1"),
            (">1.2", "1.2.9"),
            ("<1.2", "1.2.0"),
            (">*", "1.0.0"),
        ];
        for (range, version) in no {
            assert!(!satisfies(range, version), "`{range}` rejects {version}");
        }
    }

    #[test]
    fn malformed_ranges_do_not_parse() {
        for range in ["^1.2.3.4", ">=a.b", "boop", "1.2.3.4 || >=a"] {
            assert!(NpmRange::parse(range).is_none(), "`{range}` is not a range");
        }
        let with_tag = NpmRange::parse("1.0.0 || boop").expect("one set parses");
        assert!(with_tag.matches(&semver::Version::new(1, 0, 0)));
        assert!(!with_tag.matches(&semver::Version::new(1, 0, 1)));
    }

    /// A registry version as npm reads it: a leading `v` or `=` allowed,
    /// anything else that is not strict semver satisfying nothing.
    fn npm_version(text: &str) -> Option<semver::Version> {
        semver::Version::parse(text.trim().trim_start_matches(['v', '='])).ok()
    }

    /// npm's own range fixtures (node-semver `test/fixtures/range-include.js`
    /// and `range-exclude.js`), the cases without options.
    #[test]
    fn npm_range_fixtures() {
        let include: &[(&str, &str)] = &[
            ("1.0.0 - 2.0.0", "1.2.3"),
            ("^1.2.3+build", "1.2.3"),
            ("^1.2.3+build", "1.3.0"),
            ("1.2.3-pre+asdf - 2.4.3-pre+asdf", "1.2.3"),
            ("1.2.3pre+asdf - 2.4.3-pre+asdf", "1.2.3"),
            ("1.2.3-pre+asdf - 2.4.3pre+asdf", "1.2.3"),
            ("1.2.3pre+asdf - 2.4.3pre+asdf", "1.2.3"),
            ("1.2.3-pre+asdf - 2.4.3-pre+asdf", "1.2.3-pre.2"),
            ("1.2.3-pre+asdf - 2.4.3-pre+asdf", "2.4.3-alpha"),
            ("1.2.3+asdf - 2.4.3+asdf", "1.2.3"),
            ("1.0.0", "1.0.0"),
            (">=*", "0.2.4"),
            ("", "1.0.0"),
            ("*", "1.2.3"),
            ("*", "v1.2.3"),
            (">=1.0.0", "1.0.0"),
            (">=1.0.0", "1.0.1"),
            (">=1.0.0", "1.1.0"),
            (">1.0.0", "1.0.1"),
            (">1.0.0", "1.1.0"),
            ("<=2.0.0", "2.0.0"),
            ("<=2.0.0", "1.9999.9999"),
            ("<=2.0.0", "0.2.9"),
            ("<2.0.0", "1.9999.9999"),
            ("<2.0.0", "0.2.9"),
            (">= 1.0.0", "1.0.0"),
            (">=  1.0.0", "1.0.1"),
            (">=   1.0.0", "1.1.0"),
            ("> 1.0.0", "1.0.1"),
            (">  1.0.0", "1.1.0"),
            ("<=   2.0.0", "2.0.0"),
            ("<= 2.0.0", "1.9999.9999"),
            ("<=  2.0.0", "0.2.9"),
            ("<    2.0.0", "1.9999.9999"),
            ("<\t2.0.0", "0.2.9"),
            (">=0.1.97", "0.1.97"),
            ("0.1.20 || 1.2.4", "1.2.4"),
            (">=0.2.3 || <0.0.1", "0.0.0"),
            (">=0.2.3 || <0.0.1", "0.2.3"),
            (">=0.2.3 || <0.0.1", "0.2.4"),
            ("||", "1.3.4"),
            ("2.x.x", "2.1.3"),
            ("1.2.x", "1.2.3"),
            ("1.2.x || 2.x", "2.1.3"),
            ("1.2.x || 2.x", "1.2.3"),
            ("x", "1.2.3"),
            ("2.*.*", "2.1.3"),
            ("1.2.*", "1.2.3"),
            ("1.2.* || 2.*", "2.1.3"),
            ("1.2.* || 2.*", "1.2.3"),
            ("*", "1.2.3"),
            ("2", "2.1.2"),
            ("2.3", "2.3.1"),
            ("~0.0.1", "0.0.1"),
            ("~0.0.1", "0.0.2"),
            ("~x", "0.0.9"),
            ("~2", "2.0.9"),
            ("~2.4", "2.4.0"),
            ("~2.4", "2.4.5"),
            ("~>3.2.1", "3.2.2"),
            ("~1", "1.2.3"),
            ("~>1", "1.2.3"),
            ("~> 1", "1.2.3"),
            ("~1.0", "1.0.2"),
            ("~ 1.0", "1.0.2"),
            ("~ 1.0.3", "1.0.12"),
            ("~ 1.0.3alpha", "1.0.12"),
            (">=1", "1.0.0"),
            (">= 1", "1.0.0"),
            ("<1.2", "1.1.1"),
            ("< 1.2", "1.1.1"),
            ("~v0.5.4-pre", "0.5.5"),
            ("~v0.5.4-pre", "0.5.4"),
            ("=0.7.x", "0.7.2"),
            ("<=0.7.x", "0.7.2"),
            (">=0.7.x", "0.7.2"),
            ("<=0.7.x", "0.6.2"),
            ("~1.2.1 >=1.2.3", "1.2.3"),
            ("~1.2.1 =1.2.3", "1.2.3"),
            ("~1.2.1 1.2.3", "1.2.3"),
            ("~1.2.1 >=1.2.3 1.2.3", "1.2.3"),
            ("~1.2.1 1.2.3 >=1.2.3", "1.2.3"),
            (">=1.2.1 1.2.3", "1.2.3"),
            ("1.2.3 >=1.2.1", "1.2.3"),
            (">=1.2.3 >=1.2.1", "1.2.3"),
            (">=1.2.1 >=1.2.3", "1.2.3"),
            (">=1.2", "1.2.8"),
            ("^1.2.3", "1.8.1"),
            ("^0.1.2", "0.1.2"),
            ("^0.1", "0.1.2"),
            ("^0.0.1", "0.0.1"),
            ("^1.2", "1.4.2"),
            ("^1.2 ^1", "1.4.2"),
            ("^1.2.3-alpha", "1.2.3-pre"),
            ("^1.2.0-alpha", "1.2.0-pre"),
            ("^0.0.1-alpha", "0.0.1-beta"),
            ("^0.0.1-alpha", "0.0.1"),
            ("^0.1.1-alpha", "0.1.1-beta"),
            ("^x", "1.2.3"),
            ("x - 1.0.0", "0.9.7"),
            ("x - 1.x", "0.9.7"),
            ("1.0.0 - x", "1.9.7"),
            ("1.0.0 - x", "1.0.7"),
            ("1.0.0 - 1.x", "1.0.7"),
            ("1.0.0 - 1.0.x", "1.0.7"),
            ("1.x - x", "1.9.7"),
            ("<=7.x", "7.9.9"),
            (">=3.3.0-beta.1 <3.4.0-beta.3", "3.3.1"),
            ("^3.3.0-beta.1", "3.4.0"),
        ];
        let exclude: &[(&str, &str)] = &[
            ("1.0.0 - 2.0.0", "2.2.3"),
            ("1.2.3+asdf - 2.4.3+asdf", "1.2.3-pre.2"),
            ("1.2.3+asdf - 2.4.3+asdf", "2.4.3-alpha"),
            ("^1.2.3+build", "2.0.0"),
            ("^1.2.3+build", "1.2.0"),
            ("^1.2.3", "1.2.3-pre"),
            ("^1.2", "1.2.0-pre"),
            (">1.2", "1.3.0-beta"),
            ("<=1.2.3", "1.2.3-beta"),
            ("^1.2.3", "1.2.3-beta"),
            ("=0.7.x", "0.7.0-asdf"),
            (">=0.7.x", "0.7.0-asdf"),
            ("<=0.7.x", "0.7.0-asdf"),
            ("1", "1.0.0beta"),
            ("<1", "1.0.0beta"),
            ("< 1", "1.0.0beta"),
            ("1.0.0", "1.0.1"),
            (">=1.0.0", "0.0.0"),
            (">=1.0.0", "0.0.1"),
            (">=1.0.0", "0.1.0"),
            (">1.0.0", "0.0.1"),
            (">1.0.0", "0.1.0"),
            ("<=2.0.0", "3.0.0"),
            ("<=2.0.0", "2.9999.9999"),
            ("<=2.0.0", "2.2.9"),
            ("<2.0.0", "2.9999.9999"),
            ("<2.0.0", "2.2.9"),
            (">=0.1.97", "v0.1.93"),
            (">=0.1.97", "0.1.93"),
            ("0.1.20 || 1.2.4", "1.2.3"),
            (">=0.2.3 || <0.0.1", "0.0.3"),
            (">=0.2.3 || <0.0.1", "0.2.2"),
            ("2.x.x", "1.1.3"),
            ("2.x.x", "3.1.3"),
            ("1.2.x", "1.3.3"),
            ("1.2.x || 2.x", "3.1.3"),
            ("1.2.x || 2.x", "1.1.3"),
            ("2.*.*", "1.1.3"),
            ("2.*.*", "3.1.3"),
            ("1.2.*", "1.3.3"),
            ("1.2.* || 2.*", "3.1.3"),
            ("1.2.* || 2.*", "1.1.3"),
            ("2", "1.1.2"),
            ("2.3", "2.4.1"),
            ("~0.0.1", "0.1.0-alpha"),
            ("~0.0.1", "0.1.0"),
            ("~2.4", "2.5.0"),
            ("~2.4", "2.3.9"),
            ("~>3.2.1", "3.3.2"),
            ("~>3.2.1", "3.2.0"),
            ("~1", "0.2.3"),
            ("~>1", "2.2.3"),
            ("~1.0", "1.1.0"),
            ("<1", "1.0.0"),
            (">=1.2", "1.1.1"),
            ("1", "2.0.0beta"),
            ("~v0.5.4-beta", "0.5.4-alpha"),
            ("=0.7.x", "0.8.2"),
            (">=0.7.x", "0.6.2"),
            ("<0.7.x", "0.7.2"),
            ("<1.2.3", "1.2.3-beta"),
            ("=1.2.3", "1.2.3-beta"),
            (">1.2", "1.2.8"),
            ("^0.0.1", "0.0.2-alpha"),
            ("^0.0.1", "0.0.2"),
            ("^1.2.3", "2.0.0-alpha"),
            ("^1.2.3", "1.2.2"),
            ("^1.2", "1.1.9"),
            ("*", "v1.2.3-foo"),
            ("*", "not a version"),
            (">=2", "glorp"),
            ("^1.0.0", "2.0.0-rc1"),
            ("1 - 2", "2.0.0-pre"),
            ("1 - 2", "1.0.0-pre"),
            ("1.0 - 2", "1.0.0-pre"),
            ("1.1.x", "1.0.0-a"),
            ("1.1.x", "1.1.0-a"),
            ("1.1.x", "1.2.0-a"),
            ("1.x", "1.0.0-a"),
            ("1.x", "1.1.0-a"),
            ("1.x", "1.2.0-a"),
            (">=1.0.0 <1.1.0", "1.1.0"),
            (">=1.0.0 <1.1.0", "1.1.0-pre"),
            (">=1.0.0 <1.1.0-pre", "1.1.0-pre"),
            ("== 1.0.0 || foo", "2.0.0"),
            (">=3.3.0-beta.1 <3.4.0-beta.3", "3.4.5"),
        ];
        let mut wrong = Vec::new();
        for &(range, version) in include {
            let admitted = NpmRange::parse(range)
                .zip(npm_version(version))
                .is_some_and(|(range, version)| range.matches(&version));
            if !admitted {
                wrong.push(format!("`{range}` should admit {version}"));
            }
        }
        for &(range, version) in exclude {
            let admitted = NpmRange::parse(range)
                .zip(npm_version(version))
                .is_some_and(|(range, version)| range.matches(&version));
            if admitted {
                wrong.push(format!("`{range}` should reject {version}"));
            }
        }
        assert!(wrong.is_empty(), "{}", wrong.join("\n"));
    }
}
