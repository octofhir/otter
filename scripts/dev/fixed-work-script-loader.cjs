// Load the identical prepared classic Script in the engine's existing realm.
// The generated prefix captures host helpers and establishes the common shell
// environment; this lexical CommonJS loader never exposes its own fs binding.
const readFileSync = require("node:fs").readFileSync;
const sourcePath = process.argv[2];
if (!sourcePath || process.argv.length !== 3) {
  throw new Error("usage: fixed-work-script-loader.cjs <prepared-script>");
}
const source = readFileSync(sourcePath, "utf8");
(0, eval)(source);
