//! Real-page bundle measurements: what a browser embedding pays Otter to
//! load a page.
//!
//! # Contents
//! - `compile` parses and lowers every script of a saved page
//!   (`benchmarks/fixtures/web-bundles/<site>/`, fetched by
//!   `scripts/dev/fetch_web_bundles.py`) and reports parse and lowering time
//!   per script and in total.
//! - `turns` runs a page's scripts in one runtime the way a browser does
//!   (each through `run_script`, failures ignored), then times many small
//!   `run_script` turns that each call one function in the resulting heap:
//!   the shape of an event, a timer or an observer callback.
//! - `eval-turns` does the same after the page's scripts have also been
//!   compiled through indirect `eval` into live code, so the heap carries
//!   more dynamic code than any census high-water mark.
//!
//! # Invariants
//! - Scripts run in manifest (document) order.
//! - A script that does not parse as a classic script is lowered as a module;
//!   one that fails both is reported and left out of the totals.
//! - Every figure is the median of the requested number of samples.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use otter_compiler::{CompileOptions, ModuleHostInfo, compile_module_program_to_module};
use otter_runtime::{CallArgument, Runtime, SourceInput};
use otter_syntax::{SourceGoal, SourceKind};

#[derive(Debug, Parser)]
#[command(about = "Measure Otter on saved real-page script bundles")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Parse and lower every script of each site.
    Compile {
        /// Site directories (or names under benchmarks/fixtures/web-bundles).
        sites: Vec<String>,
        /// Samples per site.
        #[arg(long, default_value_t = 5)]
        samples: usize,
        /// Print every script's figures, not only the totals.
        #[arg(long)]
        verbose: bool,
        /// Compile every function body now instead of on first call.
        #[arg(long)]
        eager: bool,
    },
    /// Run a site's scripts, then time small `run_script` turns.
    Turns {
        sites: Vec<String>,
        #[arg(long, default_value_t = 2000)]
        turns: usize,
        #[arg(long, default_value_t = 3)]
        samples: usize,
        /// Run each turn through `Runtime::call` instead of a source string.
        #[arg(long)]
        call: bool,
    },
    /// As `turns`, after compiling the scripts again through `eval`.
    EvalTurns {
        sites: Vec<String>,
        #[arg(long, default_value_t = 2000)]
        turns: usize,
        #[arg(long, default_value_t = 3)]
        samples: usize,
        /// Times every script is compiled through indirect eval.
        #[arg(long, default_value_t = 2)]
        copies: usize,
    },
}

struct Script {
    name: String,
    text: String,
}

fn fixtures_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../benchmarks/fixtures/web-bundles")
}

fn site_dir(site: &str) -> PathBuf {
    let direct = PathBuf::from(site);
    if direct.join("manifest.txt").is_file() {
        direct
    } else {
        fixtures_root().join(site)
    }
}

fn load_site(site: &str) -> Vec<Script> {
    let dir = site_dir(site);
    let manifest = std::fs::read_to_string(dir.join("manifest.txt"))
        .unwrap_or_else(|err| panic!("{}: {err}", dir.display()));
    manifest
        .lines()
        .filter_map(|line| line.split('\t').next())
        .filter(|name| !name.is_empty())
        .filter_map(|name| {
            let bytes = std::fs::read(dir.join(name)).ok()?;
            Some(Script {
                name: name.to_string(),
                text: String::from_utf8_lossy(&bytes).into_owned(),
            })
        })
        .collect()
}

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1e3
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Goal {
    Script,
    Module,
}

/// Parse and lower one script; `None` when neither goal accepts it.
fn compile_once(
    script: &Script,
    goal: Goal,
    options: CompileOptions,
) -> Option<(Duration, Duration)> {
    let started = Instant::now();
    match goal {
        Goal::Script => {
            let mut parsed = Duration::ZERO;
            let lowered = otter_syntax::with_program_goal(
                &script.text,
                SourceKind::JavaScript,
                SourceGoal::Script,
                |program| {
                    parsed = started.elapsed();
                    otter_compiler::compile_script_program(
                        program,
                        SourceKind::JavaScript,
                        &script.name,
                        options,
                    )
                    .is_ok()
                },
            )
            .ok()?;
            lowered.then(|| (parsed, started.elapsed() - parsed))
        }
        Goal::Module => {
            let mut parsed = Duration::ZERO;
            let lowered = otter_syntax::with_program_goal(
                &script.text,
                SourceKind::JavaScript,
                SourceGoal::Module,
                |program| {
                    parsed = started.elapsed();
                    let url = format!("https://bench.invalid/{}", script.name);
                    let host = ModuleHostInfo {
                        module_url: url,
                        resolved_imports: Default::default(),
                    };
                    compile_module_program_to_module(
                        program,
                        SourceKind::JavaScript,
                        &host,
                        options,
                    )
                    .is_ok()
                },
            )
            .ok()?;
            lowered.then(|| (parsed, started.elapsed() - parsed))
        }
    }
}

fn run_compile(sites: &[String], samples: usize, verbose: bool, eager: bool) {
    let options = CompileOptions {
        lazy_functions: !eager,
    };
    for site in sites {
        let scripts = load_site(site);
        let mut total_parse = Duration::ZERO;
        let mut total_lower = Duration::ZERO;
        let mut total_bytes = 0usize;
        let mut failed = 0usize;
        for script in &scripts {
            let goal = if compile_once(script, Goal::Script, options).is_some() {
                Goal::Script
            } else if compile_once(script, Goal::Module, options).is_some() {
                Goal::Module
            } else {
                failed += 1;
                if verbose {
                    println!("  {:<64} FAILED", script.name);
                }
                continue;
            };
            let mut parse = Vec::with_capacity(samples);
            let mut lower = Vec::with_capacity(samples);
            for _ in 0..samples {
                let (p, l) = compile_once(script, goal, options).expect("compiled once already");
                parse.push(p);
                lower.push(l);
            }
            let (p, l) = (median(parse), median(lower));
            total_parse += p;
            total_lower += l;
            total_bytes += script.text.len();
            if verbose {
                println!(
                    "  {:<64} {:>9} B  parse {:>8.2} ms  lower {:>8.2} ms{}",
                    script.name,
                    script.text.len(),
                    ms(p),
                    ms(l),
                    if goal == Goal::Module {
                        "  (module)"
                    } else {
                        ""
                    }
                );
            }
        }
        println!(
            "{site}: {} scripts ({failed} failed), {:.2} MB  parse {:.1} ms  lower {:.1} ms  total {:.1} ms",
            scripts.len(),
            total_bytes as f64 / 1e6,
            ms(total_parse),
            ms(total_lower),
            ms(total_parse + total_lower)
        );
    }
}

/// Globals a page's scripts look for first; enough for a bundle to get
/// further than its first line without a document.
const PAGE_PRELUDE: &str = r"
var window = globalThis, self = globalThis, top = globalThis, parent = globalThis;
var __benchCounter = 0;
function __benchTick(n) { __benchCounter += n; return __benchCounter; }
";

fn page_runtime(scripts: &[Script], eval_copies: usize) -> (Runtime, Duration) {
    let mut runtime = Runtime::builder()
        .timeout(Duration::ZERO)
        .execution_stats(false)
        .process_global(false)
        .build()
        .expect("runtime builds");
    runtime
        .run_script(SourceInput::from_javascript(PAGE_PRELUDE), "<prelude>")
        .expect("prelude runs");
    let started = Instant::now();
    for script in scripts {
        let _ = runtime.run_script(SourceInput::from_javascript(&script.text), &script.name);
        let _ = runtime.run_microtasks();
    }
    for copy in 0..eval_copies {
        for (index, script) in scripts.iter().enumerate() {
            // Each copy is a distinct source so nothing can share it; the
            // body is wrapped so it is compiled and kept, never run.
            let source = format!(
                "var __benchEval{copy}_{index} = (0, eval)({:?});",
                format!(
                    "(function(){{ return function(){{ {} }}; }})()",
                    script.text
                )
            );
            let _ = runtime.run_script(SourceInput::from_javascript(&source), "<eval copy>");
        }
    }
    (runtime, started.elapsed())
}

fn time_turns(runtime: &mut Runtime, turns: usize, call: bool) -> Duration {
    if call {
        let tick = runtime.global_function("__benchTick").expect("a callable");
        let started = Instant::now();
        for turn in 0..turns {
            let step = if turn % 2 == 0 { 1.0 } else { 2.0 };
            runtime
                .call(tick, &[CallArgument::Number(step)])
                .expect("turn runs");
        }
        let elapsed = started.elapsed();
        runtime.release_function(tick);
        return elapsed;
    }
    let started = Instant::now();
    for turn in 0..turns {
        let source = if turn % 2 == 0 {
            "__benchTick(1)"
        } else {
            "__benchTick(2)"
        };
        runtime
            .run_script(SourceInput::from_javascript(source), "<turn>")
            .expect("turn runs");
        runtime.run_microtasks().expect("microtasks drain");
    }
    started.elapsed()
}

fn run_turns(sites: &[String], turns: usize, samples: usize, eval_copies: usize, call: bool) {
    for site in sites {
        let scripts = load_site(site);
        let mut loads = Vec::with_capacity(samples);
        let mut times = Vec::with_capacity(samples);
        for _ in 0..samples {
            let (mut runtime, load) = page_runtime(&scripts, eval_copies);
            loads.push(load);
            times.push(time_turns(&mut runtime, turns, call));
        }
        let (load, time) = (median(loads), median(times));
        println!(
            "{site}: load {:.1} ms, {turns} turns {:.1} ms ({:.1} us/turn)",
            ms(load),
            ms(time),
            time.as_secs_f64() * 1e6 / turns as f64
        );
    }
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::Compile {
            sites,
            samples,
            verbose,
            eager,
        } => run_compile(&sites, samples, verbose, eager),
        Command::Turns {
            sites,
            turns,
            samples,
            call,
        } => run_turns(&sites, turns, samples, 0, call),
        Command::EvalTurns {
            sites,
            turns,
            samples,
            copies,
        } => run_turns(&sites, turns, samples, copies, false),
    }
}
