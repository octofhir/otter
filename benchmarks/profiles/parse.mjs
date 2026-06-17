#!/usr/bin/env node
// Parse a samply (Firefox processed-profile) .json.gz and print top self-time
// functions. Self-time = samples whose LEAF frame is that function.
// Usage: node parse.mjs profile.json.gz [topN]
import { readFileSync } from "node:fs";
import { gunzipSync } from "node:zlib";

const [, , path, topArg] = process.argv;
const topN = topArg ? parseInt(topArg, 10) : 30;
let buf = readFileSync(path);
if (path.endsWith(".gz")) buf = gunzipSync(buf);
const prof = JSON.parse(buf.toString("utf8"));

// String table can live at top level (shared) or per-thread.
function strArr(thread) {
  if (prof.shared && prof.shared.stringArray) return prof.shared.stringArray;
  if (thread.stringArray) return thread.stringArray;
  if (thread.stringTable) return thread.stringTable; // older
  return [];
}

const self = new Map(); // funcName -> sample count
let total = 0;

// Only profile the busiest thread (the JS isolate). Idle tokio/main threads
// otherwise drown the leaf histogram in park-syscall frames.
const busiest = (prof.threads || [])
  .slice()
  .sort((a, b) => (b.samples?.length || 0) - (a.samples?.length || 0))[0];
for (const thread of busiest ? [busiest] : []) {
  const samples = thread.samples;
  if (!samples || !samples.length) continue;
  const strings = strArr(thread);
  const { stackTable, frameTable, funcTable } = thread;
  const sampleStacks = samples.stack;
  const weights = samples.weight || null;
  for (let i = 0; i < sampleStacks.length; i++) {
    const stackIdx = sampleStacks[i];
    if (stackIdx == null) continue;
    const w = weights ? Math.abs(weights[i]) : 1;
    // leaf frame of this stack
    const frameIdx = stackTable.frame[stackIdx];
    const funcIdx = frameTable.func[frameIdx];
    const nameIdx = funcTable.name[funcIdx];
    const name = strings[nameIdx] ?? `func#${funcIdx}`;
    self.set(name, (self.get(name) || 0) + w);
    total += w;
  }
}

const rows = [...self.entries()].sort((a, b) => b[1] - a[1]).slice(0, topN);
console.log(`total samples: ${total}`);
console.log(`${"pct".padStart(6)}  ${"self".padStart(8)}  function`);
for (const [name, n] of rows) {
  const pct = ((n / total) * 100).toFixed(1);
  console.log(`${pct.padStart(6)}  ${String(n).padStart(8)}  ${name}`);
}
