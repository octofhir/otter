function outer(seed) {
  let shared = { n: seed };
  function bump(delta) { shared = { n: shared.n + delta }; return shared.n; }
  let t = 0;
  for (let i = 0; i < 3000; i++) t = t + bump(1);
  return t + ":" + shared.n;
}
let r = "";
for (let k = 0; k < 30; k++) r = outer(k);
console.log(r);
