function eval() { eval(); }
let t0 = Date.now();
try { eval(); } catch (e) { console.log(String(e).slice(0, 60)); }
console.log("eval-named ms", Date.now() - t0);
