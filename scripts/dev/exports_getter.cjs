const fs = require("fs"), path = require("path");
const dir = path.join(__dirname, "exports_getter_mod");
fs.mkdirSync(dir, { recursive: true });
fs.writeFileSync(path.join(dir, "m.js"), "let n = 0; Object.defineProperty(module, 'exports', { enumerable: true, get() { return { n: ++n }; } });");
const a = require("./exports_getter_mod/m.js"), b = require("./exports_getter_mod/m.js");
console.log(a.n, b.n, a === b);
