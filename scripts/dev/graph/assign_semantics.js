// Direct-to-register assignment and compound assignment.
const log = [];
let x = 1; x += 2; log.push(x);
let y = 5; const r = (y -= 1); log.push(r, y);
let z = 3; z = z * 2 + 1; log.push(z);
let w = 0; function inc() { return 1; } w += inc(); w += inc(); log.push(w);
let a = 2; a **= 3; log.push(a);
let s = "a"; s += 1; s += {}; log.push(s);
let m = 10; const pair = [m += 1, m = 50]; log.push(pair.join("/"), m);
let p = 1; try { p = 7 || (() => { throw 1; })(); } catch (e) {} log.push(p);
let q = 0; try { q = 0 || (() => { throw "e"; })(); } catch (e) { log.push("caught " + q); }
var v = 0; try { var v2 = 3 || f(); } catch (e) {} log.push(v2);
for (let i = 0, f = function () {}; i < 1; i++) log.push(f.name);
let n = 0; for (let k = 0; k < 5; k += 2) n += k; log.push(n);
let o = { valueOf() { log.push("vo"); return 4; } }; let t = 1; t += o; log.push(t);
console.log(log.join(","));
