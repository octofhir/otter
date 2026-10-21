function T() { this.a = 1; this.b = 2; }
T.prototype.inherited = "p";
const t = new T();
for (let i = 0; i < 40; i++) t["k" + (i * 7 % 40)] = i;
t[3] = "three"; t["1"] = "one";
const out = [];
out.push(Object.keys(t).join(","));
for (const k in t) out.push(k);
out.push(JSON.stringify(t));
delete t.k5; t.k5 = "again"; t.a = 9;
out.push(Object.keys(t).slice(-3).join(","), t.inherited, t.k5, t.a, "k99" in t, t.k13);
Object.defineProperty(t, "ro", { value: 1, writable: false, enumerable: false });
t.ro = 2; out.push(t.ro, Object.getOwnPropertyNames(t).length);
let sum = 0; for (let r = 0; r < 2000; r++) for (let i = 0; i < 40; i++) sum += t["k" + i] === undefined ? 0 : 1;
out.push(sum);
console.log(out.join("|"));
