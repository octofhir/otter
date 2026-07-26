import { readFileSync } from "node:fs";
const src = readFileSync(process.argv[2], "utf8");
const engineKernel = new Function(src + "; return engineKernel;")();
for (let w = 0; w < 8; w++) engineKernel();
const out = [];
for (let i = 0; i < 20; i++) {
  const t = process.hrtime.bigint();
  engineKernel();
  out.push(Number(process.hrtime.bigint() - t) / 1e6);
}
out.sort((a, b) => a - b);
console.log(out[10].toFixed(4));
