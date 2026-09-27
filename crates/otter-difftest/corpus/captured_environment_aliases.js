// Sibling closures, mapped arguments and direct eval retain binding identity
// across collection; loop renewal replaces only its own binding. Warm generated
// entries before observing final aliases.
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

// More than the construction buffer's inline capacity, with aliases in a
// second closure. Heap capture layout and generated reads must stay identical.
function manyCaptures(seed) {
  let a0 = seed, a1 = seed + 1, a2 = seed + 2, a3 = seed + 3;
  let a4 = seed + 4, a5 = seed + 5, a6 = seed + 6, a7 = seed + 7;
  let a8 = seed + 8, a9 = seed + 9, a10 = seed + 10, a11 = seed + 11;
  let a12 = seed + 12, a13 = seed + 13, a14 = seed + 14, a15 = seed + 15;
  let a16 = seed + 16, a17 = seed + 17, a18 = seed + 18, a19 = seed + 19;
  return [
    () => a0 + a1 + a2 + a3 + a4 + a5 + a6 + a7 + a8 + a9 +
      a10 + a11 + a12 + a13 + a14 + a15 + a16 + a17 + a18 + a19,
    () => { a0++; a19 += 2; }
  ];
}
const many = manyCaptures(3);
let manyTotal = 0;
for (let i = 0; i < 128; i++) {
  manyTotal += many[0]();
  many[1]();
}
console.log(manyTotal, many[0]());
