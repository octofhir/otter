const t0 = performance.now();
const ts = require("../../benchmarks/.suite-cache/web-tooling-benchmark/node_modules/typescript/lib/typescript.js");
console.log(typeof ts.createSourceFile, (performance.now() - t0).toFixed(1) + "ms");
