#!/usr/bin/env node
// Self-time histogram for a samply profile, busiest thread only.
// Classify each leaf frame by its library:
//   - lib == "otter"  -> atos the lib-relative address against the dSYM
//   - other dylib     -> bucket under the dylib name (no symbols available)
// This avoids atos misattributing dylib/JIT addresses (mmap'd above __TEXT)
// to the last binary symbols (clap/aho-corasick).
// Usage: node symbolicate.mjs profile.json.gz [topN]
import { readFileSync } from "node:fs";
import { gunzipSync } from "node:zlib";
import { execFileSync } from "node:child_process";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = resolve(HERE, "..", "..");
const DWARF = `${ROOT}/target/release/otter.dSYM/Contents/Resources/DWARF/otter`;

const [, , path, topArg] = process.argv;
const topN = topArg ? parseInt(topArg, 10) : 25;
let buf = readFileSync(path);
if (path.endsWith(".gz")) buf = gunzipSync(buf);
const prof = JSON.parse(buf.toString("utf8"));
const libs = prof.libs || [];

const t = prof.threads.slice().sort((a, b) => (b.samples?.length || 0) - (a.samples?.length || 0))[0];
const { stackTable, frameTable, funcTable, resourceTable, samples } = t;

function libOf(funcIdx) {
  const r = funcTable.resource ? funcTable.resource[funcIdx] : -1;
  if (r < 0 || !resourceTable) return null;
  const li = resourceTable.lib ? resourceTable.lib[r] : undefined;
  return li != null && libs[li] ? libs[li].name : null;
}

// Collect: otter frames -> need atos on frame address; others -> lib bucket.
const otterAddrCount = new Map(); // hexaddr -> count
const libCount = new Map(); // libname -> count
let total = 0;
for (let i = 0; i < samples.stack.length; i++) {
  const st = samples.stack[i];
  if (st == null) continue;
  total++;
  const frameIdx = stackTable.frame[st];
  const funcIdx = frameTable.func[frameIdx];
  const lib = libOf(funcIdx);
  if (lib === "otter") {
    const addr = frameTable.address[frameIdx]; // lib-relative byte offset
    const key = "0x" + (0x100000000 + (addr >>> 0)).toString(16);
    otterAddrCount.set(key, (otterAddrCount.get(key) || 0) + 1);
  } else {
    libCount.set(lib || "(anon/jit)", (libCount.get(lib || "(anon/jit)") || 0) + 1);
  }
}

// atos all otter addresses in chunks
const addrs = [...otterAddrCount.keys()];
const resolved = new Map();
const CH = 400;
for (let i = 0; i < addrs.length; i += CH) {
  const chunk = addrs.slice(i, i + CH);
  let out;
  try { out = execFileSync("atos", ["-o", DWARF, "-l", "0x100000000", ...chunk], { encoding: "utf8" }); }
  catch { out = chunk.map(() => "?").join("\n"); }
  out.trim().split("\n").forEach((l, j) => resolved.set(chunk[j], l));
}
function fn(addr) {
  let s = resolved.get(addr) || "?";
  s = s.replace(/ \(in otter\)/, "").replace(/ \([^()]*:\d+\)\s*$/, "").replace(/::h[0-9a-f]{16}/g, "");
  s = s.replace(/\$LT\$/g, "<").replace(/\$GT\$/g, ">").replace(/\$u20\$/g, " ").replace(/\$C\$/g, ",")
       .replace(/\$RF\$/g, "&").replace(/\$u7b\$/g, "{").replace(/\$u7d\$/g, "}").replace(/\.\./g, "::");
  return s;
}

const byFn = new Map();
for (const [addr, c] of otterAddrCount) byFn.set(fn(addr), (byFn.get(fn(addr)) || 0) + c);
// dylib buckets, annotated
const libNote = { "libsystem_malloc.dylib": " «malloc/free»", "libsystem_platform.dylib": " «memcpy/memset»", "libsystem_m.dylib": " «libm: sin/cos/sqrt»" };
for (const [lib, c] of libCount) byFn.set(`[dylib] ${lib}${libNote[lib] || ""}`, c);

const otterTotal = [...otterAddrCount.values()].reduce((a, b) => a + b, 0);
console.log(`total samples: ${total}  (otter ${((otterTotal / total) * 100).toFixed(0)}%, dylib ${(100 - (otterTotal / total) * 100).toFixed(0)}%)`);
console.log(`${"pct".padStart(6)}  ${"self".padStart(6)}  function`);
for (const [n, c] of [...byFn.entries()].sort((a, b) => b[1] - a[1]).slice(0, topN))
  console.log(`${((c / total) * 100).toFixed(1).padStart(6)}  ${String(c).padStart(6)}  ${n}`);
