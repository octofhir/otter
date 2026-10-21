function P(v) { this._v = v; this.log = 0; }
Object.defineProperty(P.prototype, "v", {
  configurable: true,
  get() { return this._v; },
  set(x) { this._v = x; this.log++; },
});
function run(o, n) {
  let s = 0;
  for (let i = 0; i < n; i++) { o.v = o.v + 1; s += o.v; }
  return s;
}
const p = new P(1);
console.log(run(p, 50000), p.log);
Object.defineProperty(P.prototype, "v", { configurable: true, get() { return 7; }, set(x) { this.log += 10; } });
console.log(run(p, 50000), p.log);
console.log(run(new P(3), 50000));
