// A direct eval anywhere in the module gives every closure created by the
// module body a dynamic eval environment. Callees that cannot observe that
// environment still inline; a callee that deopts inside its spliced body
// resumes with its closure's environment; a callee that resolves names
// through the environment keeps seeing the eval-introduced binding.
function P(a, b) { this.a = a; this.b = b; }
function cons(a, b) { return new P(a, b); }
function add(x, y) { return x + y; }
function widen(x) { return x * 3 + 0.5; }
function useEval(src) { return eval(src); }
function viaEval(k) {
  eval("var hidden = " + k);
  return function () { return hidden + k; };
}

let total = 0;
for (let round = 0; round < 3000; round++) {
  let list = null;
  for (let i = 0; i < 40; i++) list = cons(i, list);
  total = add(total, list.a) | 0;
  // Int32 speculation fails inside the spliced body after warm-up.
  total += round < 2500 ? widen(round) | 0 : Math.floor(widen(round + 0.25));
  if ((round & 255) === 0) total += viaEval(round)();
}
console.log(total, useEval("1+1"), useEval("typeof cons"));
