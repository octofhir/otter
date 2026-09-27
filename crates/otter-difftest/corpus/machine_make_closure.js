// Closures created inside hot optimized loops capture the live cells of
// their activation: later writes are visible, each iteration's block cell is
// fresh, arrows keep lexical `this`, and a named function sees itself.
function build(n) {
  let total = 0;
  const fns = [];
  for (let i = 0; i < n; i++) {
    let local = i * 2;
    const add = (x) => x + local + total;
    fns.push(add);
    local += 1;
    total += add(1) & 7;
  }
  let sum = 0;
  for (let k = 0; k < fns.length; k += 97) sum += fns[k](k);
  return sum;
}

function Counter() {
  this.base = 5;
  let calls = 0;
  for (let i = 0; i < 3000; i++) {
    const bump = () => { calls++; return this.base + calls; };
    bump();
  }
  return calls;
}

function named(depth) {
  let seen = 0;
  for (let i = 0; i < 3000; i++) {
    const self = function inner() { return inner === self; };
    if (self()) seen++;
  }
  return seen + depth;
}

let out = [];
for (let round = 0; round < 4; round++) {
  out.push(build(4000));
  out.push(new Counter().base, Counter.call({ base: 1 }));
  out.push(named(round));
}
console.log(out.join(","));

// Capture-free functions and RegExp literals created in a hot loop are
// distinct objects on every evaluation.
function freshObjects(n) {
  let distinctFns = 0;
  let distinctRes = 0;
  let matched = 0;
  let prevFn = null;
  let prevRe = null;
  for (let i = 0; i < n; i++) {
    const f = function (x) { return x + 1; };
    const re = /a(b+)c/g;
    if (f !== prevFn) distinctFns++;
    if (re !== prevRe) distinctRes++;
    if (re.lastIndex === 0 && re.test("xabbbc")) matched += f(re.lastIndex);
    prevFn = f;
    prevRe = re;
  }
  return [distinctFns, distinctRes, matched].join(":");
}
let fresh = "";
for (let round = 0; round < 3; round++) fresh = freshObjects(5000);
console.log(fresh);
