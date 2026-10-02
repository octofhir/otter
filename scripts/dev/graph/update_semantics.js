// Update / unary / compound semantics across coercion paths.
let log = [];
const o = { valueOf() { log.push("v"); return 5; } };
let a = o; let r1 = a++; log.push(r1, a);
let b = o; let r2 = ++b; log.push(r2, b);
let c = o; c--; log.push(c);
let d = 10n; d++; log.push(String(d));
let e = "7"; let r3 = e++; log.push(r3, e);
let f = o; f += 1; log.push(f);
let g = { valueOf() { log.push("g"); return 3; } };
let h = { valueOf() { log.push("h"); return 4; } };
let gg = g; gg -= h; log.push(gg);
log.push(-o, ~o, +o);
try { let s = Symbol(); s++; } catch (err) { log.push(err.constructor.name); }
let obj = { p: o }; let r4 = obj.p++; log.push(r4, obj.p);
let arr = [o]; ++arr[0]; log.push(arr[0]);
let i = 0, j = 10; for (; i < 3; i++, j--) {} log.push(i, j);
let k = 1; let r5 = (k++, k++); log.push(r5, k);
function sum(n) { let s = 0; for (let x = 0; x < n; x++) s += x; return s; }
let t = 0; for (let z = 0; z < 2000; z++) t += sum(100); log.push(t);
let m = 0; let r6 = m++ + (m = 10); log.push(r6, m);
let q = 0; let r7 = ++q + (q = 10); log.push(r7, q);
console.log(log.join(","));
