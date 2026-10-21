function g() { g(); }
let t0 = Date.now();
try { g(); } catch (e) { console.log(String(e).slice(0, 60)); }
console.log("plain-recursion ms", Date.now() - t0);
function eval() { eval(); }
t0 = Date.now();
try { eval(); } catch (e) { console.log(String(e).slice(0, 60)); }
console.log("eval-named ms", Date.now() - t0);
