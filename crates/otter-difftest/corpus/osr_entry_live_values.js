// A loop entered through OSR keeps every live value it received from the
// interpreter frame: the induction variable, the captured constructor and the
// elided `arguments` reads, while the loop body constructs through an inlined
// constructor. A lost register copy walks the induction variable out of range
// and never terminates, so the loop is bounded and checked per call.
function Pair(car, cdr) { this.car = car; this.cdr = cdr; }

function list() {
  var res = null;
  var a = arguments;
  for (var i = a.length - 1, n = 0; i >= 0 && n < 8; i--, n++)
    res = new Pair(a[i], res);
  return res;
}

function render(p) {
  const parts = [];
  for (; p !== null && parts.length < 10; p = p.cdr) parts.push(String(p.car));
  return parts.join(",");
}

let bad = 0;
let first = "";
for (let k = 0; k < 3000; k++) {
  const got = render(list(k, 1, 2));
  if (got !== k + ",1,2") {
    bad++;
    if (first === "") first = k + " => " + got;
  }
}

let wide = 0;
for (let k = 0; k < 2000; k++) {
  const p = list(k, k + 1, k + 2, k + 3, k + 4);
  wide += p.car + p.cdr.cdr.cdr.cdr.car;
}
console.log("osr_entry_live_values", bad, first, wide);
