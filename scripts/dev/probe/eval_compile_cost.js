// Compile cost of a large source through indirect eval, direct eval in a
// function, and `new Function`, against the size of the text. The source is
// wrapped so evaluating it only declares one function and runs nothing.
const fs = require("fs");
const path = process.env.EVAL_SOURCE || "benchmarks/fixtures/web-bundles/www.theverge.com/23-32y27qdl4n320.js";
const body = fs.readFileSync(path, "utf8");
const src = "(function(){" + body + "\n})";
function time(name, run) {
  const t = performance.now();
  run();
  const ms = performance.now() - t;
  console.log(name.padEnd(14), ms.toFixed(1), "ms", (ms * 1000 / (body.length / 1024)).toFixed(1), "us/KB");
}
time("indirect eval", () => (0, eval)(src));
time("direct eval", function () { return eval(src); });
time("new Function", () => new Function(src));
// The text as a page evaluates it: its top level runs, so its immediately
// invoked bodies compile too. It stops at its first missing browser global.
time("eval as is", () => { try { (0, eval)(body); } catch (e) { console.log("  stopped:", String(e).slice(0, 60)); } });
// A text held as two-byte code units, as a decoded network response may be.
const wide = body + "\n// €";
time("eval two-byte", () => { try { (0, eval)(wide); } catch (e) {} });
