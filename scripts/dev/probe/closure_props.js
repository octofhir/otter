function Base(x) { this.x = x; }
function Derived(x) { Base.call(this, x); }
function* gen() { yield 1; }
const out = [];
let s = 0;
for (let i = 0; i < 100000; i++) {
  const d = new Derived(i);
  s += d.x;
  const f = i & 1 ? Base : Derived;
  if (f.call !== Function.prototype.call) s = -1;
  if (f.apply !== Function.prototype.apply) s = -2;
  s += f.length + f.name.length;
  if (f.nope !== undefined) s = -3;
  if (gen.call !== Function.prototype.call) s = -4;
}
out.push(s);
Base.extra = 7;
let t = 0;
for (let i = 0; i < 1000; i++) t += Base.extra + (Base.call === Function.prototype.call ? 1 : 0);
out.push(t);
Function.prototype.zzz = 5;
let u = 0;
for (let i = 0; i < 1000; i++) u += Derived.zzz;
out.push(u);
Derived.call = function () { return 42; };
out.push(Derived.call());
Object.defineProperty(Base, "name", { value: "Renamed" });
out.push(Base.name, Derived.name, Derived.length, typeof Derived.prototype);
console.log(out.join(","));
