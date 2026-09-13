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
  (function() {
    const tag = 8;
    return function eight(a) {
      function live() { return a; }
      return tag + live() + arguments[1] + arguments.length +
        (arguments.length > 2 ? arguments[2].value : 0);
    };
  })()
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
const thrown = { marker: 17 };
function throwTarget(a) {
  if (arguments.length !== 3 || arguments[2].value !== 7) throw new Error('lost throw actuals');
  throw thrown;
}
for (let i = 0; i < 128; i++) {
  try { throwTarget(i, 2, { value: 7 }); }
  catch (error) { if (error !== thrown) throw error; }
}
selected = throwTarget;
let caught = 0;
for (let i = 0; i < 128; i++) {
  try { forward(i, 2, { value: 7 }); }
  catch (error) { if (error !== thrown) throw new Error('lost native throw'); caught++; }
}
if (caught !== 128) throw new Error('missing native throw');
let customEffects = 0;
throwTarget.apply = function(receiver, list) {
  if (list.length !== 3 || list[2].value !== 7) throw new Error('lost custom actuals');
  customEffects++;
  throw thrown;
};
caught = 0;
for (let i = 0; i < 128; i++) {
  try { forward(i, 2, { value: 7 }); }
  catch (error) { if (error !== thrown) throw new Error('lost custom throw'); caught++; }
}
if (caught !== 128 || customEffects !== 128) throw new Error('replayed custom apply');
function restTarget(...args) { return args[0] + args.length + args[2].value; }
selected = restTarget;
for (let i = 0; i < 64; i++) {
  if (forward(i, 2, { value: 7 }) !== i + 11) throw new Error('rest admission');
}
function evalTarget(a) { eval('a += 1'); return a; }
selected = evalTarget;
for (let i = 0; i < 64; i++) {
  if (forward(i, 2, { value: 7 }) !== i + 2) throw new Error('eval admission');
}
let constructorEffects = 0;
class CannotCall { constructor() { constructorEffects++; } }
selected = CannotCall;
let rejected = false;
try { forward(0, 2, { value: 7 }); }
catch (error) { rejected = error instanceof TypeError; }
if (!rejected || constructorEffects !== 0) throw new Error('class call admission');
JSON.stringify([checksum, settled]);
