//! `otter-regex-bench` — a measurement harness for the RegExp engine.
//!
//! The engine has no other thermometer: without one, a lowering or scan change
//! is guesswork. This binary runs a fixed landscape of patterns over generated
//! subjects and reports throughput, so two builds can be compared directly.
//!
//! # Contents
//! - `--suite` (default) — the built-in pattern landscape.
//! - `--pattern P [--flags F] [--input FILE]` — one ad-hoc measurement.
//! - `--dump P [--flags F]` — print the lowered program for a pattern.
//!
//! # Invariants
//! - Generated corpora come from a fixed-seed LCG, so the same subject bytes
//!   are measured on every run and across machines.
//! - Each case batches passes until a batch outlasts the clock resolution, then
//!   repeats for at least `--min-time` milliseconds and reports the best
//!   per-pass time observed.
//! - The summary row reports the summed per-pass time across cases and the
//!   geometric mean of their throughputs.
//!
//! # See also
//! - `crates/otter-regex/src/api.rs` — the surface being measured.

use std::time::{Duration, Instant};

use otter_regex::{ExecConfig, Flags, Regex};

/// One measurable case: a pattern, its flags, and the corpus it runs against.
struct Case {
    /// Human-readable label printed in the results table.
    name: &'static str,
    /// The pattern source.
    pattern: &'static str,
    /// The flag string (`i`, `u`, `v`, `m`, `s`).
    flags: &'static str,
    /// Which generated corpus this case scans.
    corpus: Corpus,
}

/// The generated subject a case scans.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Corpus {
    /// Mixed ASCII prose: words, spaces, punctuation, newlines.
    Prose,
    /// Prose with a sparse sprinkling of non-BMP code points.
    Astral,
    /// Densely repeated `a` runs — the adversarial shape for give-back.
    Runs,
    /// Line-oriented log records with timestamps and levels.
    Logs,
}

impl Corpus {
    /// The label printed alongside a case.
    fn name(self) -> &'static str {
        match self {
            Self::Prose => "prose",
            Self::Astral => "astral",
            Self::Runs => "runs",
            Self::Logs => "logs",
        }
    }
}

/// The built-in landscape. Each entry targets a distinct engine path, so a
/// change that helps one shape and hurts another is visible rather than hidden
/// in an average.
const CASES: &[Case] = &[
    // Leftmost scan: how fast can the engine reject positions?
    Case {
        name: "literal-short",
        pattern: "quick",
        flags: "",
        corpus: Corpus::Prose,
    },
    Case {
        name: "literal-long",
        pattern: "unquestionable",
        flags: "",
        corpus: Corpus::Prose,
    },
    Case {
        name: "literal-icase",
        pattern: "Quick",
        flags: "i",
        corpus: Corpus::Prose,
    },
    Case {
        name: "alt-literals",
        pattern: "quick|quiet|quilt",
        flags: "",
        corpus: Corpus::Prose,
    },
    Case {
        name: "alt-shared-prefix",
        pattern: "transaction|transmission|transition",
        flags: "",
        corpus: Corpus::Prose,
    },
    // Anchors: the scan should not retry every position.
    Case {
        name: "anchored-miss",
        pattern: "^zzzzz",
        flags: "",
        corpus: Corpus::Prose,
    },
    Case {
        name: "anchored-alt",
        pattern: "^foo|^bar",
        flags: "",
        corpus: Corpus::Prose,
    },
    Case {
        name: "anchored-multiline",
        pattern: "^ERROR",
        flags: "m",
        corpus: Corpus::Logs,
    },
    // Class repeats: the fused-repeat scan.
    Case {
        name: "word-runs",
        pattern: "\\w+",
        flags: "",
        corpus: Corpus::Prose,
    },
    Case {
        name: "word-runs-u",
        pattern: "\\w+",
        flags: "u",
        corpus: Corpus::Prose,
    },
    Case {
        name: "class-runs-u",
        pattern: "[a-z]+",
        flags: "u",
        corpus: Corpus::Prose,
    },
    Case {
        name: "digits-u",
        pattern: "[0-9]{2,}",
        flags: "u",
        corpus: Corpus::Logs,
    },
    Case {
        name: "not-space",
        pattern: "[^ \\n]{4,}",
        flags: "",
        corpus: Corpus::Prose,
    },
    // Give-back pressure.
    Case {
        name: "run-giveback",
        pattern: "a+b",
        flags: "",
        corpus: Corpus::Runs,
    },
    Case {
        name: "run-giveback-u",
        pattern: "a+b",
        flags: "u",
        corpus: Corpus::Runs,
    },
    // Structured extraction with captures.
    Case {
        name: "log-record",
        pattern: "(\\d{4})-(\\d{2})-(\\d{2}) (\\w+) ",
        flags: "",
        corpus: Corpus::Logs,
    },
    Case {
        name: "quoted",
        pattern: "\"[^\"]*\"",
        flags: "",
        corpus: Corpus::Logs,
    },
    // Unicode traversal over a subject that actually contains surrogate pairs.
    Case {
        name: "astral-class",
        pattern: "[\\u{1F300}-\\u{1FAFF}]",
        flags: "u",
        corpus: Corpus::Astral,
    },
    Case {
        name: "astral-word",
        pattern: "\\w+",
        flags: "u",
        corpus: Corpus::Astral,
    },
    // Lookaround and backreference paths.
    Case {
        name: "lookahead",
        pattern: "\\w+(?= )",
        flags: "",
        corpus: Corpus::Prose,
    },
    Case {
        name: "lookbehind",
        pattern: "(?<= )\\w+",
        flags: "",
        corpus: Corpus::Prose,
    },
    Case {
        name: "backref",
        pattern: "(\\w)\\1",
        flags: "",
        corpus: Corpus::Prose,
    },
];

/// A fixed-seed linear congruential generator, so every run measures the same
/// bytes.
struct Lcg(u64);

impl Lcg {
    /// Next pseudo-random 32-bit value.
    fn next(&mut self) -> u32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (self.0 >> 33) as u32
    }

    /// Next value in `0..n`.
    fn below(&mut self, n: u32) -> u32 {
        self.next() % n
    }
}

/// The words the prose corpora are built from. A handful of the case patterns
/// hit rarely, which is the point: a scan that only goes fast on dense matches
/// is not going fast.
const WORDS: &[&str] = &[
    "the",
    "quick",
    "brown",
    "fox",
    "jumps",
    "over",
    "lazy",
    "dog",
    "transaction",
    "transmission",
    "transition",
    "quiet",
    "quilt",
    "unquestionable",
    "value",
    "record",
    "index",
    "buffer",
    "stream",
    "handle",
    "offset",
    "length",
    "pattern",
    "subject",
    "matcher",
    "program",
    "aa",
    "ll",
    "oo",
    "ee",
];

/// Build a corpus of at least `target_units` UTF-16 code units.
fn build_corpus(kind: Corpus, target_units: usize) -> Vec<u16> {
    let mut rng = Lcg(0x0DDB_A11B_ADC0_FFEE);
    let mut s = String::new();
    match kind {
        Corpus::Prose | Corpus::Astral => {
            let mut col = 0;
            while s.encode_utf16().count() < target_units {
                let w = WORDS[rng.below(WORDS.len() as u32) as usize];
                s.push_str(w);
                col += w.len() + 1;
                if kind == Corpus::Astral && rng.below(64) == 0 {
                    // A sparse astral code point: enough to exercise the
                    // surrogate-pair path without dominating the corpus.
                    let cp = 0x1F300 + rng.below(0x400);
                    s.push(char::from_u32(cp).unwrap_or('\u{1F300}'));
                }
                if col > 68 {
                    s.push('\n');
                    col = 0;
                } else if rng.below(16) == 0 {
                    s.push_str(", ");
                } else {
                    s.push(' ');
                }
            }
        }
        Corpus::Runs => {
            while s.encode_utf16().count() < target_units {
                let n = 1 + rng.below(40);
                for _ in 0..n {
                    s.push('a');
                }
                // `b` appears rarely, so `a+b` mostly fails after consuming a
                // whole run — exactly the give-back shape worth measuring.
                s.push(if rng.below(8) == 0 { 'b' } else { 'c' });
            }
        }
        Corpus::Logs => {
            const LEVELS: &[&str] = &["INFO", "WARN", "ERROR", "DEBUG"];
            while s.encode_utf16().count() < target_units {
                let level = LEVELS[rng.below(LEVELS.len() as u32) as usize];
                s.push_str(&format!(
                    "{:04}-{:02}-{:02} {} request id={} took={}ms \"{}\"\n",
                    2020 + rng.below(6),
                    1 + rng.below(12),
                    1 + rng.below(28),
                    level,
                    rng.next(),
                    rng.below(5000),
                    WORDS[rng.below(WORDS.len() as u32) as usize],
                ));
            }
        }
    }
    s.encode_utf16().collect()
}

/// The outcome of measuring one case.
struct Measurement {
    /// Matches found in a single full scan of the subject.
    matches: usize,
    /// Best observed wall time for one full scan.
    best: Duration,
    /// Code units scanned per pass.
    units: usize,
}

impl Measurement {
    /// Millions of code units scanned per second.
    fn mcups(&self) -> f64 {
        self.units as f64 / self.best.as_secs_f64() / 1e6
    }
}

/// Scan `text` end to end, returning the match count.
fn scan(re: &Regex, text: &[u16]) -> usize {
    let mut n = 0;
    for m in re.find_utf16(text, 0, ExecConfig::default()) {
        match m {
            Ok(_) => n += 1,
            Err(_) => break,
        }
    }
    n
}

/// Time `reps` consecutive scans, asserting each finds `expected` matches so
/// the work cannot be optimized away.
fn timed_batch(re: &Regex, text: &[u16], reps: u32, expected: usize) -> Duration {
    let t = Instant::now();
    for _ in 0..reps {
        assert_eq!(scan(re, text), expected);
    }
    t.elapsed()
}

/// Measure one compiled pattern against one subject.
///
/// A single scan can finish faster than the clock resolves — an anchored
/// pattern rejects the whole subject in a handful of instructions — so passes
/// are batched until a batch is long enough to time, and the reported figure is
/// the best per-pass time across batches.
fn measure(re: &Regex, text: &[u16], min_time: Duration) -> Measurement {
    let matches = scan(re, text);
    let mut reps = 1u32;
    while timed_batch(re, text, reps, matches) < Duration::from_millis(5) && reps < (1 << 20) {
        reps *= 4;
    }
    let mut best = Duration::MAX;
    let started = Instant::now();
    let mut batches = 0u32;
    while started.elapsed() < min_time || batches < 3 {
        best = best.min(timed_batch(re, text, reps, matches) / reps);
        batches += 1;
    }
    Measurement {
        matches,
        best,
        units: text.len(),
    }
}

/// Run the built-in landscape.
fn run_suite(min_time: Duration, filter: Option<&str>, corpus_units: usize) {
    println!(
        "{:<22} {:<8} {:<8} {:>10} {:>12} {:>10}",
        "case", "flags", "corpus", "matches", "ns/pass", "Mcu/s"
    );
    println!("{}", "-".repeat(74));
    let mut cached: Vec<(Corpus, Vec<u16>)> = Vec::new();
    let mut total_ns = 0u128;
    let mut log_sum = 0.0f64;
    let mut counted = 0u32;
    for case in CASES {
        if let Some(f) = filter
            && !case.name.contains(f)
        {
            continue;
        }
        if !cached.iter().any(|(k, _)| *k == case.corpus) {
            cached.push((case.corpus, build_corpus(case.corpus, corpus_units)));
        }
        let text = &cached.iter().find(|(k, _)| *k == case.corpus).unwrap().1;
        let re = match Regex::compile_str(case.pattern, Flags::from_str_lossy(case.flags)) {
            Ok(re) => re,
            Err(e) => {
                println!("{:<22} COMPILE ERROR: {e}", case.name);
                continue;
            }
        };
        let m = measure(&re, text, min_time);
        println!(
            "{:<22} {:<8} {:<8} {:>10} {:>12} {:>10.1}",
            case.name,
            if case.flags.is_empty() {
                "-"
            } else {
                case.flags
            },
            case.corpus.name(),
            m.matches,
            m.best.as_nanos(),
            m.mcups(),
        );
        total_ns += m.best.as_nanos();
        log_sum += m.mcups().ln();
        counted += 1;
    }
    if counted > 0 {
        // Two aggregates, because neither alone is honest. The summed pass time
        // is the figure a whole-engine change moves, but it hides anything that
        // is already fast; the geometric mean weights every case equally and so
        // survives a case that drops to near-zero work.
        println!("{}", "-".repeat(74));
        println!(
            "{:<22} {:<8} {:<8} {:>10} {:>12} {:>10.1}",
            "TOTAL / GEOMEAN",
            "",
            "",
            "",
            total_ns,
            (log_sum / f64::from(counted)).exp()
        );
    }
}

/// Print the parsed usage help.
fn usage() {
    eprintln!(
        "otter-regex-bench — RegExp engine measurement harness

  otter-regex-bench [--suite] [--filter SUBSTR] [--min-time MS] [--corpus-units N]
  otter-regex-bench --pattern P [--flags F] [--input FILE] [--min-time MS]
  otter-regex-bench --dump P [--flags F]

Options:
  --suite               Run the built-in pattern landscape (the default).
  --filter SUBSTR       Only run suite cases whose name contains SUBSTR.
  --min-time MS         Minimum measurement time per case (default 300).
  --corpus-units N      Generated corpus size in UTF-16 code units (default 1048576).
  --pattern P           Measure one ad-hoc pattern instead of the suite.
  --flags F             Flag string for --pattern / --dump.
  --input FILE          Subject file for --pattern (default: generated prose).
  --dump P              Print the lowered program for pattern P and exit."
    );
}

/// Entry point.
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut min_time = Duration::from_millis(300);
    let mut corpus_units = 1 << 20;
    let mut filter: Option<String> = None;
    let mut pattern: Option<String> = None;
    let mut dump: Option<String> = None;
    let mut flags = String::new();
    let mut input: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        let take = |i: &mut usize| -> String {
            *i += 1;
            args.get(*i).cloned().unwrap_or_default()
        };
        match args[i].as_str() {
            "--suite" => {}
            "--filter" => filter = Some(take(&mut i)),
            "--min-time" => min_time = Duration::from_millis(take(&mut i).parse().unwrap_or(300)),
            "--corpus-units" => corpus_units = take(&mut i).parse().unwrap_or(1 << 20),
            "--pattern" => pattern = Some(take(&mut i)),
            "--dump" => dump = Some(take(&mut i)),
            "--flags" => flags = take(&mut i),
            "--input" => input = Some(take(&mut i)),
            "-h" | "--help" => {
                usage();
                return;
            }
            other => {
                eprintln!("unknown argument: {other}");
                usage();
                std::process::exit(2);
            }
        }
        i += 1;
    }

    let parsed_flags = Flags::from_str_lossy(&flags);

    if let Some(p) = dump {
        match Regex::compile_str(&p, parsed_flags) {
            Ok(re) => print!("{}", re.describe()),
            Err(e) => {
                eprintln!("compile error: {e}");
                std::process::exit(1);
            }
        }
        return;
    }

    if let Some(p) = pattern {
        let text: Vec<u16> = match &input {
            Some(path) => match std::fs::read_to_string(path) {
                Ok(s) => s.encode_utf16().collect(),
                Err(e) => {
                    eprintln!("cannot read {path}: {e}");
                    std::process::exit(1);
                }
            },
            None => build_corpus(Corpus::Prose, corpus_units),
        };
        let re = match Regex::compile_str(&p, parsed_flags) {
            Ok(re) => re,
            Err(e) => {
                eprintln!("compile error: {e}");
                std::process::exit(1);
            }
        };
        let m = measure(&re, &text, min_time);
        println!(
            "pattern  /{p}/{flags}\nsubject  {} code units\nmatches  {}\nns/pass  {}\nMcu/s    {:.1}",
            m.units,
            m.matches,
            m.best.as_nanos(),
            m.mcups()
        );
        return;
    }

    run_suite(min_time, filter.as_deref(), corpus_units);
}
