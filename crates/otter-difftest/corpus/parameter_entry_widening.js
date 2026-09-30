// Optimized entries speculate parameter representations from the uses they
// reach, including uses on untaken paths. A missing argument, a double or a
// string at a failed entry guard must widen only that parameter, and results
// must match the interpreter throughout.

function sum3(a, b, c) {
  return a + b + (c === undefined ? 0 : c);
}
function under(n) {
  return sum3(n, 2);
}
let t = 0;
for (let i = 0; i < 40000; i++) t += under(i);
console.log(t);

function scale(x, k) {
  return x * k + 1;
}
let s = 0;
for (let i = 0; i < 40000; i++) s += scale(i, 3);
for (let i = 0; i < 40000; i++) s += scale(i, 0.5);
console.log(s);

function label(x, suffix) {
  return x + 1 + (suffix === undefined ? 0 : suffix);
}
let out = 0;
for (let i = 0; i < 40000; i++) out += label(i);
console.log(out, label(1, "!"), label(2.5), label(3, 4));
let again = 0;
for (let i = 0; i < 40000; i++) again += label(i, 1);
console.log(again);
