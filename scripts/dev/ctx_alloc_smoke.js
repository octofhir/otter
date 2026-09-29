// Context allocation smoke: closures capturing per-call and per-iteration
// scopes, TDZ holes, copies across loop iterations, and GC pressure.
function make(n) {
  const fs = [];
  for (let i = 0; i < n; i++) {
    let x = i * 2;
    fs.push(() => x + i);
  }
  return fs;
}
function outer(a) {
  let b = a + 1;
  function inner(c) {
    let d = c + b;
    return () => d + a + b;
  }
  return inner(a * 3)();
}
function tdz() {
  const out = [];
  for (let k = 0; k < 3; k++) {
    let captured;
    out.push(() => captured);
    captured = k;
  }
  return out.map((f) => f()).join(',');
}
let sum = 0;
for (let round = 0; round < 2000; round++) {
  const fs = make(50);
  for (const f of fs) sum += f();
  sum += outer(round);
  if (round % 97 === 0) {
    const junk = [];
    for (let j = 0; j < 2000; j++) junk.push({ j, s: 'x' + j });
    sum += junk.length;
  }
}
console.log(sum, tdz());
