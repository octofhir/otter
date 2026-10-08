// Extract the emscripten "use asm" module source from the zlib fixture's eval string.
import fs from "node:fs";
const text = fs.readFileSync(new URL("../../../../benchmarks/fixtures/fixed-work/zlib.js", import.meta.url), "utf8");
const start = text.indexOf('zlibEval("') + 'zlibEval('.length;
let end = start + 1;
while (text[end] !== '"') end += text[end] === "\\" ? 2 : 1;
const code = JSON.parse(text.slice(start, end + 1));
fs.writeFileSync(new URL("zlib_payload.js", import.meta.url), code);
const asmAt = code.indexOf('"use asm"');
let fnStart = code.lastIndexOf("function", asmAt);
fs.writeFileSync(new URL("zlib_asm_head.txt", import.meta.url), code.slice(fnStart - 200, asmAt + 4000));
console.log(code.length, asmAt, fnStart);
