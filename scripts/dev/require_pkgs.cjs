const base = "../../benchmarks/.suite-cache/web-tooling-benchmark/node_modules/";
const t0 = performance.now();
for (const p of ["@babel/core", "acorn", "ajv", "postcss", "source-map", "esprima", "chai"]) {
  try { require(base + p); } catch (e) { console.log(p, "failed:", String(e.message).slice(0, 80)); }
}
console.log((performance.now() - t0).toFixed(1) + "ms");
