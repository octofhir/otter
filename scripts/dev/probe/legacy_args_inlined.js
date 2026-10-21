// fn.arguments and fn.caller read while the function runs inlined (no deopt).
let seen = 0, bad = 0, callers = 0;
function inspect(fn) {
  const list = fn.arguments;
  if (list === null) { bad++; return 0; }
  if (list.length !== 3 || list[2] !== 'x') bad++;
  if (fn.caller === outer) callers++;
  seen++;
  return list[1];
}
function target(value) {
  if ((value & 7) === 0) return inspect(target) + value;
  return value + 1;
}
function outer(v) { return target(v, v * 2, 'x'); }
let sum = 0;
for (let i = 0; i < 40000; i++) sum += outer(i);
console.log(JSON.stringify([sum, seen, bad, callers]));
