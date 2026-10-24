// Named loads on every primitive receiver kind once the site runs in generated code.
Symbol.prototype.custom = 1; Number.prototype.custom = 2; Boolean.prototype.custom = 3; BigInt.prototype.custom = 4; String.prototype.custom = 5;
Object.defineProperty(Symbol.prototype, "getterOnSym", { get() { return typeof this; } });
const vals = [Symbol("x"), 1.5, 7, true, 10n, "s"];
const names = ["toString", "valueOf", "custom", "constructor", "description", "getterOnSym", "missing"];
function load(v, n) { return v[n]; }
function out() {
  return vals.map(v => names.map(n => { const r = load(v, n); return typeof r === "function" ? "fn" : String(r); }).join(",")).join(" | ");
}
const first = out();
for (let i = 0; i < 20000; i++) out();
const last = out();
console.log(first === last ? "same" : "DIFF\n" + first + "\n" + last);
function named(v) { return [typeof v.toString, typeof v.valueOf, v.custom, typeof v.constructor].join(); }
const nf = vals.map(named).join(" | ");
for (let i = 0; i < 20000; i++) vals.forEach(named);
const nl = vals.map(named).join(" | ");
console.log(nf === nl ? "same" : "DIFF\n" + nf + "\n" + nl);
