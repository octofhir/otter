// Legacy `fn.arguments` and `fn.caller` read while the activation runs
// inlined in an optimized caller, one and two levels deep, with constant
// and computed actuals, and after the activation returned.
let seen = 0, bad = 0, callers = 0, innerCallers = 0;
function inspect(fn, expectCaller) {
  const list = fn.arguments;
  if (list === null) { bad++; return 0; }
  if (list.length !== 3 || list[2] !== "x") bad++;
  if (fn.caller === expectCaller) callers++;
  seen++;
  return list[1];
}
function target(value) {
  if ((value & 7) === 0) return inspect(target, outer) + value;
  return value + 1;
}
function outer(v) { return target(v, v * 2, "x"); }
function middle(v) { return deeper(v, v + 1, "x"); }
function deeper(v) {
  if ((v & 15) === 0) {
    const list = deeper.arguments;
    if (list && list[1] === v + 1 && deeper.caller === middle) innerCallers++;
  }
  return v;
}
function top(v) { return middle(v) + 1; }
let sum = 0;
for (let i = 0; i < 40000; i++) sum += outer(i) + top(i);
console.log(JSON.stringify([sum, seen, bad, callers, innerCallers, target.arguments, deeper.caller]));
