//! Shared constructor-root workloads for moving-GC correctness and measurement.
//!
//! # Contents
//! - [`AllocationFixture`] selects fixed or spread construction.
//! - [`AllocationFixture::source`] preserves the retained-object workload while
//!   varying only the getter's allocation count.
//! - A separate observed setup brackets the same allocation loop for native
//!   root/source proofs without changing benchmark source or kernel bytes.
//! - Explicit setup/probe phases let correctness tests exclude warmup GC while
//!   the diagnostic probe executes their unchanged concatenation.
//! - Kernel emission wraps the same source for the existing engine benchmark.
//!
//! # Invariants
//! - Each fixture warms its constructor 5,000 times before a proxy's observable
//!   prototype read retains the requested number of objects and strings.
//! - The final completion verifies the argument, receiver prototype, and exact
//!   retained-object count. Scaling never removes those checks.
//!
//! # See also
//! - `crates/otter-benchmark/src/bin/allocation_probe.rs` measures each fixture/tier.

/// Observable constructor preparation with fixed or spread arguments.
#[derive(Clone, Copy)]
pub enum AllocationFixture {
    Fixed,
    Spread,
}

impl AllocationFixture {
    /// Stable workload name used by machine-readable measurements.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Fixed => "construct",
            Self::Spread => "spread",
        }
    }

    /// Generate one complete fixture, retaining every allocation in its sink.
    #[allow(dead_code, reason = "the diagnostic probe runs the full concatenation")]
    pub fn source(self, allocations: usize) -> String {
        self.setup(allocations) + self.probe()
    }

    /// Wrap the unchanged workload in an engine kernel with semantic checks.
    #[allow(dead_code, reason = "kernel emission belongs to the diagnostic probe")]
    pub fn kernel(self, allocations: usize) -> String {
        let (prototype, sink) = match self {
            Self::Fixed => (
                "constructGcPrototype",
                "globalThis.__machineConstructGcSink",
            ),
            Self::Spread => ("spreadGcPrototype", "globalThis.__spreadGcSink"),
        };
        format!(
            "function engineKernel() {{\n{}\n\
             if (result.marker !== \"kept:42\" ||\n\
                 Object.getPrototypeOf(result) !== {prototype} ||\n\
                 {sink}.length !== {allocations}) {{\n\
               throw new Error(\"reentrant allocation invariant failed\");\n\
             }}\n\
             return {sink}.length;\n\
             }}\n",
            self.source(allocations),
        )
    }

    /// Declare the fixture and warm generated constructor linkage.
    pub fn setup(self, allocations: usize) -> String {
        self.setup_template(allocations, false)
    }

    /// Add scalar observations around the getter's original allocation loop.
    ///
    /// The runtime regression installs `constructorGcObserve`; the benchmark
    /// continues to use `setup`, whose emitted bytes are unchanged.
    #[allow(dead_code, reason = "only the runtime proof installs an observer")]
    pub fn setup_observed(self, allocations: usize) -> String {
        self.setup_template(allocations, true)
    }

    fn setup_template(self, allocations: usize, observed: bool) -> String {
        let before = if observed {
            "      constructorGcObserve(0);\n"
        } else {
            ""
        };
        let after = if observed {
            "      constructorGcObserve(1);\n"
        } else {
            ""
        };
        // These are source templates, not text-based JavaScript parsing. The
        // benchmark varies only its decimal bound; the separate observed
        // variant adds scalar hooks around the same allocation loop.
        match self {
            Self::Fixed => format!(
                r#"
let constructGcProbe = false;
const constructGcPrototype = {{ marker: "prototype" }};
globalThis.__machineConstructGcSink = [];

function GcBaseTarget(marker) {{
  this.marker = marker;
}}

const GcBase = new Proxy(GcBaseTarget, {{
  get(target, key, receiver) {{
    if (key !== "prototype") return Reflect.get(target, key, receiver);
    if (constructGcProbe) {{
{before}      for (let i = 0; i < {allocations}; i++) {{
        globalThis.__machineConstructGcSink.push({{ i, padding: "construct-gc-" + i }});
      }}
{after}    }}
    return constructGcPrototype;
  }}
}});

function constructGc(Ctor, marker) {{
  return new Ctor(marker);
}}

for (let i = 0; i < 5000; i++) constructGc(GcBase, "warm:" + i);
"#
            ),
            Self::Spread => format!(
                r#"
let spreadGcProbe = false;
const spreadGcPrototype = {{ marker: "spread-gc-prototype" }};
globalThis.__spreadGcSink = [];

function SpreadGcBaseTarget(marker) {{
  this.marker = marker;
}}

const SpreadGcBase = new Proxy(SpreadGcBaseTarget, {{
  get(target, key, receiver) {{
    if (key !== "prototype") return Reflect.get(target, key, receiver);
    if (spreadGcProbe) {{
{before}      for (let i = 0; i < {allocations}; i++) {{
        globalThis.__spreadGcSink.push({{ i, padding: "spread-gc-" + i }});
      }}
{after}    }}
    return spreadGcPrototype;
  }}
}});

function constructSpreadGc(Ctor, args) {{
  return new Ctor(...args);
}}

const warmArgs = ["warm"];
for (let i = 0; i < 5000; i++) constructSpreadGc(SpreadGcBase, warmArgs);
"#
            ),
        }
    }

    /// Trigger the allocating getter while receiver/argument roots are live.
    pub const fn probe(self) -> &'static str {
        match self {
            Self::Fixed => {
                r#"constructGcProbe = true;
const marker = "kept:" + 42;
const result = constructGc(GcBase, marker);
JSON.stringify([
  result.marker,
  Object.getPrototypeOf(result) === constructGcPrototype,
  globalThis.__machineConstructGcSink.length
]);
"#
            }
            Self::Spread => {
                r#"spreadGcProbe = true;
const liveArgs = ["kept:42"];
const result = constructSpreadGc(SpreadGcBase, liveArgs);
JSON.stringify([
  result.marker,
  Object.getPrototypeOf(result) === spreadGcPrototype,
  globalThis.__spreadGcSink.length
]);
"#
            }
        }
    }

    /// Exact observable completion, shared by benchmarks and correctness tests.
    pub fn expected_completion(allocations: usize) -> String {
        format!(r#"["kept:42",true,{allocations}]"#)
    }
}
