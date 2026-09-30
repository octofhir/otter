// Residual generated calls from spliced bodies: the inline parents exist only
// in the call site's recipe, yet stack traces, callee deopts and throws must
// see the same activations as the interpreter.
// Function names and line:column of the first four frames (the last one is
// the script, whose name differs between engines).
function names(stack) {
  return stack
    .split("\n")
    .slice(1, 5)
    .map((line, index) => {
      const position = line.match(/:(\d+:\d+)\)?$/)[1];
      return index < 3 ? line.trim().split(" ")[1] + "@" + position : position;
    })
    .join(",");
}

function leaf(n) {
  if (n === 7) return names(new Error("x").stack);
  return n + 1;
}
function mid(n) {
  return leaf(n);
}
function outer(n) {
  return mid(n);
}
let s = 0;
for (let i = 0; i < 20000; i++) s += outer(i & 3);
console.log(s, outer(7));

function leaf2(x) {
  return x.a + 1;
}
function mid2(x) {
  return leaf2(x) * 2;
}
function outer2(x) {
  return mid2(x) + 1;
}
let t = 0;
for (let i = 0; i < 20000; i++) t += outer2({ a: i });
console.log(t, outer2({ a: 1.5 }), outer2({ b: 1, a: "s" }));

function thrower(n) {
  if (n > 19990) throw new Error("boom" + n);
  return n;
}
function mid3(n) {
  return thrower(n) + 1;
}
function outer3(n) {
  try {
    return mid3(n);
  } catch (e) {
    return e.message + ":" + names(e.stack);
  }
}
let u = 0;
let last = "";
for (let i = 0; i < 20000; i++) {
  const r = outer3(i);
  if (typeof r === "number") u += r;
  else last = r;
}
console.log(u, last);

function Point(x) {
  this.x = x;
}
function makePoint(n) {
  return new Point(n).x;
}
function sumPoints(n) {
  return makePoint(n) + makePoint(n + 1);
}
let p = 0;
for (let i = 0; i < 20000; i++) p += sumPoints(i);
console.log(p);
