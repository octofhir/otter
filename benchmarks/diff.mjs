#!/usr/bin/env node
// OtterLab differential runner — proves every benchmarks/scripts/* produces
// IDENTICAL stdout across Otter execution tiers. This is the Phase-0 correctness
// gate: a perf change that silently alters a result must fail here, not ship.
//
// Configs compared (all the same otter binary, different env):
//   interp     OTTER_JIT=0                                  interpreter only
//   jit        OTTER_JIT=1                                  PupJIT baseline
//   jit-osr    OTTER_JIT=1 OTTER_JIT_OSR_THRESHOLD=1        forced early loop OSR
//
// The interpreter is the correctness oracle; jit and jit-osr must match it
// byte-for-byte on stdout. Exit code is non-zero if ANY script disagrees, so
// the command is usable as a CI gate / `just` recipe.
//
// Usage:
//   node benchmarks/diff.mjs                  # all scripts, all configs
//   node benchmarks/diff.mjs fib json         # substring filter
//   node benchmarks/diff.mjs --timeout 30000
//   node benchmarks/diff.mjs --json out.json  # also write raw json here
//
// Output: markdown table to stdout + results/diff-latest.{json,md}.

import { spawnSync } from "node:child_process";
import { readdirSync, mkdirSync, writeFileSync, existsSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join, resolve } from "node:path";

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, "..");
const SCRIPTS_DIR = join(HERE, "scripts");
const RESULTS_DIR = join(HERE, "results");

function otterBin() {
  for (const p of [
    join(ROOT, "target/release/otter"),
    join(ROOT, "target/debug/otter"),
  ]) {
    if (existsSync(p)) return p;
  }
  return null;
}

// The three Otter execution configs to cross-check. `oracle` is the config every
// other one must match; the interpreter is the spec oracle.
const CONFIGS = [
  { name: "interp", env: { OTTER_JIT: "0" }, oracle: true },
  { name: "jit", env: { OTTER_JIT: "1" } },
  { name: "jit-osr", env: { OTTER_JIT: "1", OTTER_JIT_OSR_THRESHOLD: "1" } },
];

// ---- arg parsing ----------------------------------------------------------
const argv = process.argv.slice(2);
let timeout = 120000;
let jsonOut = null;
const filters = [];
for (let i = 0; i < argv.length; i++) {
  const a = argv[i];
  if (a === "--timeout") timeout = parseInt(argv[++i], 10);
  else if (a === "--json") jsonOut = argv[++i];
  else if (a.startsWith("--")) {
    console.error(`unknown flag: ${a}`);
    process.exit(2);
  } else filters.push(a);
}

const bin = otterBin();
if (!bin) {
  console.error("otter binary not found — build it first: cargo build --release -p otter-cli");
  process.exit(2);
}

let scripts = readdirSync(SCRIPTS_DIR)
  .filter((f) => /\.(js|mjs|ts)$/.test(f) && !f.startsWith("_"))
  .sort();
if (filters.length) scripts = scripts.filter((s) => filters.some((f) => s.includes(f)));
if (!scripts.length) {
  console.error("no matching scripts");
  process.exit(2);
}

// ---- run one (script, config) ---------------------------------------------
function runOnce(file, cfg) {
  const r = spawnSync(bin, ["run", file], {
    encoding: "utf8",
    stdio: ["ignore", "pipe", "pipe"],
    timeout,
    env: { ...process.env, ...cfg.env },
  });
  if (r.signal === "SIGTERM") return { ok: false, err: `timeout >${timeout}ms` };
  if (r.status !== 0 || r.error) {
    return {
      ok: false,
      err: (r.error && r.error.message) || (r.stderr || "").trim() || `exit ${r.status}`,
    };
  }
  // Normalize only trailing whitespace/newlines; interior output must match.
  return { ok: true, out: (r.stdout || "").replace(/\s+$/, "") };
}

// ---- run all --------------------------------------------------------------
const rows = [];
let anyFail = false;
for (const s of scripts) {
  const file = join(SCRIPTS_DIR, s);
  const outs = {};
  for (const cfg of CONFIGS) outs[cfg.name] = runOnce(file, cfg);

  const oracle = CONFIGS.find((c) => c.oracle).name;
  const base = outs[oracle];
  let status = "ok";
  let detail = "";

  if (!base.ok) {
    status = "ERROR";
    detail = `${oracle}: ${base.err}`;
  } else {
    for (const cfg of CONFIGS) {
      const r = outs[cfg.name];
      if (!r.ok) {
        status = "ERROR";
        detail = `${cfg.name}: ${r.err}`;
        break;
      }
      if (r.out !== base.out) {
        status = "MISMATCH";
        detail = `${cfg.name}=${JSON.stringify(r.out)} != ${oracle}=${JSON.stringify(base.out)}`;
        break;
      }
    }
  }

  if (status !== "ok") anyFail = true;
  rows.push({ script: s, status, detail, value: base.ok ? base.out : null });
  process.stderr.write(
    `${s.padEnd(24)} ${status}${status !== "ok" ? "  " + detail : "  = " + base.out}\n`,
  );
}

// ---- report ---------------------------------------------------------------
let md = `# Differential output equality (Otter tiers)\n\n`;
md += `Each script's stdout compared across \`${CONFIGS.map((c) => c.name).join("`, `")}\`. `;
md += `\`interp\` is the oracle; \`jit\` / \`jit-osr\` must match it exactly.\n\n`;
md += `Host: \`${process.platform} ${process.arch}\` · node harness \`${process.version}\`\n\n`;
md += `| script | result | value / detail |\n| --- | --- | --- |\n`;
for (const r of rows) {
  const cell = r.status === "ok" ? "✅ ok" : r.status === "MISMATCH" ? "❌ MISMATCH" : "⚠️ ERROR";
  md += `| ${r.script} | ${cell} | ${r.status === "ok" ? "`" + r.value + "`" : r.detail} |\n`;
}
const passed = rows.filter((r) => r.status === "ok").length;
md += `\n**${passed}/${rows.length} identical across all tiers.**\n`;
md += `\n_Generated ${new Date().toISOString()}_\n`;

console.log("\n" + md);

mkdirSync(RESULTS_DIR, { recursive: true });
writeFileSync(join(RESULTS_DIR, "diff-latest.md"), md);
const raw = {
  generatedAt: new Date().toISOString(),
  platform: process.platform,
  arch: process.arch,
  configs: CONFIGS.map((c) => ({ name: c.name, env: c.env, oracle: !!c.oracle })),
  total: rows.length,
  passed,
  rows,
};
writeFileSync(join(RESULTS_DIR, "diff-latest.json"), JSON.stringify(raw, null, 2));
if (jsonOut) writeFileSync(resolve(jsonOut), JSON.stringify(raw, null, 2));
console.error(`\nwrote benchmarks/results/diff-latest.{md,json}`);

process.exit(anyFail ? 1 : 0);
