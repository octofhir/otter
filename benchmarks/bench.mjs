#!/usr/bin/env node
// Benchmark harness: runs each script in benchmarks/scripts/ across every
// configured JS runtime, measures wall-clock, and prints a comparison table
// normalized to Otter. Pure-JS workloads only (no fs/net/node APIs).
//
// Usage:
//   node benchmarks/bench.mjs                 # all scripts, all runtimes
//   node benchmarks/bench.mjs fib nbody       # only matching scripts
//   node benchmarks/bench.mjs --runs 20       # override timed runs
//   node benchmarks/bench.mjs --only otter,node
//   node benchmarks/bench.mjs --json out.json # also write raw json
//
// Output: markdown table to stdout + results/latest.{json,md}.

import { spawnSync, execSync } from "node:child_process";
import { readdirSync, mkdirSync, writeFileSync, existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join, resolve, extname } from "node:path";

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, "..");
const SCRIPTS_DIR = join(HERE, "scripts");
const RESULTS_DIR = join(HERE, "results");

// ---- runtime registry -----------------------------------------------------
// Each runtime: how to turn a script path into argv. `tsArgs` lets a runtime
// run .ts files directly (node needs a flag; deno/bun are native).
function otterBin() {
  for (const p of [
    join(ROOT, "target/release/otter"),
    join(ROOT, "target/debug/otter"),
  ]) {
    if (existsSync(p)) return p;
  }
  return null;
}

// Resolve the installed `node` version once and cache whether it strips
// TypeScript types without a flag (v23.6+, or the v22.18+ LTS backport).
let _nodeStrips = null;
function nodeStripsTypesByDefault() {
  if (_nodeStrips !== null) return _nodeStrips;
  try {
    const v = execSync("node --version", { encoding: "utf8" }).trim().replace(/^v/, "");
    const [major, minor] = v.split(".").map((n) => parseInt(n, 10));
    _nodeStrips = major > 23 || (major === 23 && minor >= 6) || (major === 22 && minor >= 18);
  } catch {
    _nodeStrips = false;
  }
  return _nodeStrips;
}

const RUNTIMES = [
  {
    name: "otter",
    bin: otterBin(),
    argv: (f) => [otterBin(), ["run", f]],
    ts: true, // otter parses TS via oxc natively
    env: { OTTER_JIT: "0" }, // interpreter only (baseline)
  },
  {
    name: "otter-jit",
    bin: otterBin(),
    argv: (f) => [otterBin(), ["run", f]],
    ts: true,
    env: { OTTER_JIT: "1" }, // baseline JIT enabled
  },
  {
    name: "otter-jit-osr",
    bin: otterBin(),
    argv: (f) => [otterBin(), ["run", f]],
    ts: true,
    // Forced early loop OSR: tier up on the first back-edge so the OSR/compiled
    // loop path is exercised even on short workloads. Opt-in (see --only); not
    // in the default set so the headline table stays interp vs jit vs externals.
    env: { OTTER_JIT: "1", OTTER_JIT_OSR_THRESHOLD: "1" },
    optIn: true,
  },
  {
    name: "node",
    bin: "node",
    // Node strips TypeScript types by default from v23.6 (and v22.18 LTS).
    // On those, run `.ts` directly; older nodes need the explicit flag.
    argv: (f) =>
      extname(f) === ".ts" && !nodeStripsTypesByDefault()
        ? ["node", ["--experimental-strip-types", f]]
        : ["node", [f]],
    ts: true,
  },
  {
    name: "deno",
    bin: "deno",
    argv: (f) => ["deno", ["run", "--quiet", "--allow-read", f]],
    ts: true,
  },
  {
    name: "bun",
    bin: "bun",
    argv: (f) => ["bun", ["run", f]],
    ts: true,
  },
];

// ---- arg parsing ----------------------------------------------------------
const argv = process.argv.slice(2);
let runs = 10;
let warmup = 2;
let timeout = 60000; // per-run kill switch (ms); a hung/slow runtime fails fast
let onlyRuntimes = null;
let jsonOut = null;
const filters = [];
for (let i = 0; i < argv.length; i++) {
  const a = argv[i];
  if (a === "--runs") runs = parseInt(argv[++i], 10);
  else if (a === "--warmup") warmup = parseInt(argv[++i], 10);
  else if (a === "--timeout") timeout = parseInt(argv[++i], 10);
  else if (a === "--only") onlyRuntimes = argv[++i].split(",").map((s) => s.trim());
  else if (a === "--json") jsonOut = argv[++i];
  else if (a.startsWith("--")) {
    console.error(`unknown flag: ${a}`);
    process.exit(1);
  } else filters.push(a);
}

// ---- runtime availability -------------------------------------------------
function detect(rt) {
  if (rt.name === "otter") return !!rt.bin;
  try {
    execSync(`command -v ${rt.bin}`, { stdio: "ignore" });
    return true;
  } catch {
    return false;
  }
}

let runtimes = RUNTIMES.filter(detect);
if (onlyRuntimes) runtimes = runtimes.filter((r) => onlyRuntimes.includes(r.name));
else runtimes = runtimes.filter((r) => !r.optIn); // opt-in tiers (forced OSR) only via --only
if (!runtimes.length) {
  console.error("no runtimes available");
  process.exit(1);
}

// ---- script discovery -----------------------------------------------------
let scripts = readdirSync(SCRIPTS_DIR)
  .filter((f) => /\.(js|mjs|ts)$/.test(f) && !f.startsWith("_"))
  .sort();
if (filters.length)
  scripts = scripts.filter((s) => filters.some((f) => s.includes(f)));
if (!scripts.length) {
  console.error("no matching scripts");
  process.exit(1);
}

// ---- timing ---------------------------------------------------------------
function timeOnce(bin, args, env) {
  const t0 = process.hrtime.bigint();
  const r = spawnSync(bin, args, {
    encoding: "utf8",
    stdio: ["ignore", "ignore", "pipe"],
    timeout,
    env: env ? { ...process.env, ...env } : process.env,
  });
  const t1 = process.hrtime.bigint();
  if (r.signal === "SIGTERM") return { ok: false, ms: NaN, err: `timeout >${timeout}ms` };
  if (r.status !== 0 || r.error) {
    return { ok: false, ms: NaN, err: (r.error && r.error.message) || r.stderr || `exit ${r.status}` };
  }
  return { ok: true, ms: Number(t1 - t0) / 1e6 };
}

function measure(rt, file) {
  const [bin, args] = rt.argv(file);
  for (let i = 0; i < warmup; i++) {
    const w = timeOnce(bin, args, rt.env);
    if (!w.ok) return { ok: false, err: w.err };
  }
  const samples = [];
  for (let i = 0; i < runs; i++) {
    const m = timeOnce(bin, args, rt.env);
    if (!m.ok) return { ok: false, err: m.err };
    samples.push(m.ms);
  }
  samples.sort((a, b) => a - b);
  const min = samples[0];
  const mean = samples.reduce((a, b) => a + b, 0) / samples.length;
  const median = samples[(samples.length / 2) | 0];
  const sd = Math.sqrt(samples.reduce((a, b) => a + (b - mean) ** 2, 0) / samples.length);
  return { ok: true, min, mean, median, sd, samples };
}

// ---- run ------------------------------------------------------------------
console.error(
  `runtimes: ${runtimes.map((r) => r.name).join(", ")} | runs=${runs} warmup=${warmup}\n`,
);

const results = {}; // script -> runtime -> result
for (const s of scripts) {
  const file = join(SCRIPTS_DIR, s);
  results[s] = {};
  process.stderr.write(`${s.padEnd(24)}`);
  for (const rt of runtimes) {
    const res = measure(rt, file);
    results[s][rt.name] = res;
    process.stderr.write(res.ok ? ` ${rt.name}=${res.min.toFixed(1)}ms` : ` ${rt.name}=FAIL`);
  }
  process.stderr.write("\n");
}

// ---- report ---------------------------------------------------------------
const rtNames = runtimes.map((r) => r.name);
const hasOtter = rtNames.includes("otter");

function fmt(res) {
  if (!res || !res.ok) return "FAIL";
  return `${res.min.toFixed(1)}`;
}
// How many times slower the reference engine (base) is than this runtime.
// >1 = base slower than the runtime; <1 = base FASTER than the runtime.
function ratio(res, base) {
  if (!res || !res.ok || !base || !base.ok) return "";
  return `${(base.min / res.min).toFixed(2)}×`;
}

// The shipping engine is `otter-jit` (the baseline JIT is on by default);
// anchor every ratio to it so the table surfaces "our engine vs node" — the
// number that matters — instead of the interpreter-only baseline.
const refName = rtNames.includes("otter-jit") ? "otter-jit" : "otter";

let md = `# Benchmark results\n\n`;
md += `Metric: **min wall-clock ms** over ${runs} runs (${warmup} warmup), lower is better. `;
md += `Includes process startup. \`×\` = how many times **slower \`${refName}\` is** than that runtime — `;
md += `**\`<1.00×\` means \`${refName}\` is faster.**\n\n`;
md += `Host: \`${process.platform} ${process.arch}\` · node harness \`${process.version}\`\n\n`;

const header = ["script", ...rtNames.flatMap((n) => (n !== refName ? [n, `${n} ×`] : [n]))];
md += `| ${header.join(" | ")} |\n`;
md += `| ${header.map(() => "---").join(" | ")} |\n`;

for (const s of scripts) {
  const row = [s];
  const base = results[s][refName];
  for (const n of rtNames) {
    const res = results[s][n];
    row.push(fmt(res));
    if (n !== refName) row.push(ratio(res, base));
  }
  md += `| ${row.join(" | ")} |\n`;
}

md += `\n_Generated ${new Date().toISOString()}_\n`;

console.log("\n" + md);

mkdirSync(RESULTS_DIR, { recursive: true });
writeFileSync(join(RESULTS_DIR, "latest.md"), md);
const raw = { generatedAt: new Date().toISOString(), runs, warmup, platform: process.platform, arch: process.arch, results };
writeFileSync(join(RESULTS_DIR, "latest.json"), JSON.stringify(raw, null, 2));
if (jsonOut) writeFileSync(resolve(jsonOut), JSON.stringify(raw, null, 2));
console.error(`\nwrote ${join("benchmarks/results", "latest.md")} + latest.json`);
