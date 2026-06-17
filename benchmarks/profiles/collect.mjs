#!/usr/bin/env node
// Drive otter over every bench script in JIT-on and JIT-off modes, capturing
// min wall-clock + the OTTER_STATS=1 counter snapshot. Emits a markdown table
// + raw json to stdout.
import { spawnSync } from "node:child_process";
import { readdirSync } from "node:fs";
import { join, dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, "..", "..");
const BIN = join(ROOT, "target/release/otter");
const SCRIPTS = join(ROOT, "benchmarks/scripts");
const RUNS = 4;

const scripts = readdirSync(SCRIPTS).filter((f) => /\.(js|ts)$/.test(f) && !f.startsWith("_")).sort();

function runOnce(file, jit) {
  const env = { ...process.env, OTTER_STATS: "1" };
  env.OTTER_JIT = jit ? "1" : "0"; // default is ON; OTTER_JIT=0 forces interp
  const r = spawnSync(BIN, ["run", join(SCRIPTS, file)], { encoding: "utf8", env, timeout: 120000 });
  if (r.status !== 0) return { ok: false, err: (r.stderr || "").slice(-200) };
  // stats json is the last JSON line on stderr
  const lines = (r.stderr || "").trim().split("\n");
  let stats = null, dur = NaN;
  for (let i = lines.length - 1; i >= 0; i--) {
    try { const o = JSON.parse(lines[i]); if (o.schema === "otter.stats.v1") { stats = o.stats; dur = o.durationMs; break; } } catch {}
  }
  return { ok: true, dur, stats };
}

function measure(file, jit) {
  let best = null;
  for (let i = 0; i < RUNS; i++) {
    const r = runOnce(file, jit);
    if (!r.ok) return r;
    if (best === null || r.dur < best.dur) best = r;
  }
  return { ok: true, ...best };
}

const out = {};
for (const s of scripts) {
  process.stderr.write(s.padEnd(24));
  const off = measure(s, false);
  const on = measure(s, true);
  out[s] = { off, on };
  process.stderr.write(` off=${off.ok ? off.dur.toFixed(0) : "FAIL"} on=${on.ok ? on.dur.toFixed(0) : "FAIL"}\n`);
}

// Markdown
const f = (x) => (x == null ? "" : x);
let md = "# Otter internal: JIT off vs on + counters (durationMs, min of " + RUNS + ")\n\n";
md += "| script | off ms | on ms | speedup | propHit | propMiss | propDisable | jitDirect | jitRtCall | jitFallback | gcMB | gcCyc | gcPauseMs |\n";
md += "|---|---|---|---|---|---|---|---|---|---|---|---|---|\n";
for (const s of scripts) {
  const { off, on } = out[s];
  if (!on.ok) { md += `| ${s} | ${off.ok ? off.dur.toFixed(0) : "FAIL"} | FAIL: ${on.err} |\n`; continue; }
  const st = on.stats || {};
  const sp = off.ok ? (off.dur / on.dur).toFixed(2) + "×" : "";
  const gcMB = ((st.gcAllocBytesTotal || 0) / 1048576).toFixed(1);
  md += `| ${s} | ${off.ok ? off.dur.toFixed(0) : "FAIL"} | ${on.dur.toFixed(0)} | ${sp} | ${f(st.propertyLoadHits)} | ${f(st.propertyLoadMisses)} | ${f(st.propertyLoadDisables)} | ${f(st.jitDirectCalls)} | ${f(st.jitRuntimeCalls)} | ${f(st.jitRustCallFallbacks)} | ${gcMB} | ${f(st.gcCycles)} | ${f(st.gcLastPauseMs)} |\n`;
}
console.log(md);
console.log("\n```json\n" + JSON.stringify(out, null, 1) + "\n```");
