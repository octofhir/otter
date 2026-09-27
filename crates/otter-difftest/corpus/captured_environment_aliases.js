// Several bindings share one moving environment. Sibling closures, mapped
// arguments and direct eval must retain binding identity; loop renewal replaces
// only its own reference. Warm generated entries before observing final aliases.
function make(seed) {
  let left = { n: seed };
  let right = { n: seed + 10 };
  let calls = 0;
  return [
    function read() { return left.n + right.n + calls; },
    function update(n) { left = { n: n }; right = { n: n + 10 }; calls++; }
  ];
}
function drive(pair, n) {
  let total = 0;
  for (let i = 0; i < n; i++) {
    pair[1](i);
    total += pair[0]();
  }
  return total;
}
const pair = make(1);
console.log(drive(pair, 512), pair[0]());
function loopBindings() {
  let shared = 1;
  const readers = [];
  for (let i = 0; i < 4; i++) {
    let own = { n: i };
    readers.push(function () { own.n++; return [i, own.n, shared++].join(":"); });
  }
  return readers.map(f => f()).join("|") + "/" + readers.map(f => f()).join("|");
}
console.log(loopBindings());
function mapped(a, b) {
  const args = arguments;
  const read = () => a.n + b.n;
  args[0] = { n: 7 };
  b = { n: 11 };
  return [read(), args[0].n, args[1].n];
}
console.log(mapped({n: 1}, {n: 2}).join(":"));
function dynamic() {
  let a = { n: 3 }, b = { n: 5 };
  const read = () => a.n + b.n;
  eval("a = { n: 13 }; b = { n: 17 }");
  return read;
}
console.log(dynamic()());
function tdz() {
  const read = () => x;
  let result;
  try { read(); } catch (e) { result = e.name; }
  let x = 23;
  return result + ":" + read();
}
console.log(tdz());
