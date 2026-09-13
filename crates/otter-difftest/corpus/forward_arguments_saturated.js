// Distinct bytecode bodies saturate ordinary-call feedback; factory closures do not.
const targets = [
  function zero(a) { return a + arguments[1] + arguments.length; },
  function one(a) { return 1 + a + arguments[1] + arguments.length; },
  function two(a) { return 2 + a + arguments[1] + arguments.length; },
  function three(a) { return 3 + a + arguments[1] + arguments.length; },
  function four(a) { return 4 + a + arguments[1] + arguments.length; },
  function five(a) { return 5 + a + arguments[1] + arguments.length; },
  function six(a) { return 6 + a + arguments[1] + arguments.length; },
  function seven(a) { return 7 + a + arguments[1] + arguments.length; },
  function eight(a) { return 8 + a + arguments[1] + arguments.length; }
];
let selected = targets[0];
function forward(a, b) { a++; return selected.apply(null, arguments); }
let checksum = 0;
for (let target = 0; target < targets.length; target++) {
  selected = targets[target];
  for (let i = 0; i < 256; i++) checksum += forward(i, 2);
}
let settled = 0;
for (let i = 0; i < 512; i++) {
  selected = targets[i % targets.length];
  settled += forward(i, 2);
}
JSON.stringify([checksum, settled]);
