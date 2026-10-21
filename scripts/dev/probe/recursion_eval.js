function eval() { eval(); }
function DoWhile_3() { eval(); }
let t0 = Date.now();
try { DoWhile_3(); } catch (e) { console.log(String(e).slice(0, 60)); }
let t1 = Date.now();
var r;
function* f() { r = arguments; test(); yield 170; }
let depth = 0;
function test() {
  function foopy() {
    try { for (var i of f()); } catch (e) { depth++; }
  }
  foopy();
}
test();
let t2 = Date.now();
console.log("eval-recursion ms", t1 - t0, "generator-recursion ms", t2 - t1, "catches", depth);
