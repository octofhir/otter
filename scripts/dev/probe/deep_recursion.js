// Deep recursion: RangeError, catchable, engine stays usable; stack trace depth.
function f(n) { return n === 0 ? 0 : 1 + f(n - 1); }
let depth = 0;
function g() { depth++; g(); }
let caught = "none";
try { g(); } catch (e) { caught = e instanceof RangeError ? "RangeError" : String(e); }
const ok = f(10000);
function h(n) { if (n === 0) throw new Error("x"); return h(n - 1); }
let frames = 0;
try { h(20); } catch (e) { frames = e.stack.split("\n").filter(l => l.includes(" h ") || l.includes("at h")).length; }
console.log(caught, depth > 1000, ok, frames);
