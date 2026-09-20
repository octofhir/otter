//! Shared constructor-root workloads for moving-GC correctness and measurement.
//!
//! # Contents
//! - [`AllocationFixture`] selects fixed or spread construction.
//! - [`AllocationFixture::source`] preserves the retained-object workload while
//!   varying only the getter's allocation count.
//! - Explicit setup/probe phases let correctness tests exclude warmup GC while
//!   the diagnostic probe executes their unchanged concatenation.
//! - Kernel emission wraps the same source for the existing engine benchmark.
//!
//! # Invariants
//! - Each fixture warms its constructor 5,000 times before one observable
//!   prototype getter retains the requested number of objects and strings.
//! - The final completion verifies the argument, receiver prototype, and exact
//!   retained-object count. Scaling never removes those checks.
//!
//! # See also
//! - `crates/otter-benchmark/src/bin/allocation_probe.rs` measures each fixture/tier.
//! - `crates/otter-runtime/tests/jit_machine_direct_call.rs` owns generated-linkage assertions.

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
        // These are source templates, not text-based JavaScript parsing. Only
        // the decimal loop bound changes relative to the correctness fixture.
        match self {
            Self::Fixed => format!(
                r#"
let constructGcProbe = false;
const constructGcPrototype = {{ marker: "prototype" }};
globalThis.__machineConstructGcSink = [];

function GcBase(marker) {{
  this.marker = marker;
}}

Object.defineProperty(GcBase, "prototype", {{
  configurable: true,
  get() {{
    if (constructGcProbe) {{
      for (let i = 0; i < {allocations}; i++) {{
        globalThis.__machineConstructGcSink.push({{ i, padding: "construct-gc-" + i }});
      }}
    }}
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

function SpreadGcBase(marker) {{
  this.marker = marker;
}}

Object.defineProperty(SpreadGcBase, "prototype", {{
  configurable: true,
  get() {{
    if (spreadGcProbe) {{
      for (let i = 0; i < {allocations}; i++) {{
        globalThis.__spreadGcSink.push({{ i, padding: "spread-gc-" + i }});
      }}
    }}
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
