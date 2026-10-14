// Storage kinds remain immutable through callbacks, layout changes and GC.
// Lengths stay live, and every receiver replacement needs its own proof.
const results = [];
let effects = 0;
const retained = [];
const prototypes = [];
for (let i = 0; i < 8; i++) prototypes.push({ inherited: i + 4 });

function mutateBorn(array, object, seed) {
  effects++;
  array.push(seed + 2);
  object.value = seed + 3;
  Object.setPrototypeOf(object, prototypes[seed]);
  retained[seed & 15] = { array, object, keep: 'x'.repeat(80) + seed };
}
function bornValues(seed) {
  const array = [seed, seed + 1];
  const object = { value: seed };
  const before = array.length + object.value;
  mutateBorn(array, object, seed);
  return before + array.length + object.value + object.inherited;
}
let total = 0;
for (let i = 0; i < 16000; i++) total += bornValues(i & 7);
results.push(total, effects, retained[7].array.length, retained[7].object.value);

function intrinsicBirths(value) {
  const empty = Array();
  const sized = new Array(3);
  const object = Object();
  object.value = value;
  return empty.length + sized.length + object.value;
}
total = 0;
for (let i = 0; i < 16000; i++) total += intrinsicBirths(i & 7);
results.push(total);

let lengthEffects = 0;
function touchLength(array, grow) {
  lengthEffects++;
  if (grow) array.length = 2147483648;
  else array.push(5);
  retained[lengthEffects & 15] = { array, other: new Array(3), keep: { value: lengthEffects } };
}
function readAroundEffect(array, grow) {
  const before = array.length;
  touchLength(array, grow);
  return before + array.length;
}
total = 0;
for (let i = 0; i < 16000; i++) total += readAroundEffect([1, 2], false);
const huge = [1, 2];
results.push(total, readAroundEffect(huge, true), huge.length, lengthEffects);

function chooseReceiver(flag, array, other) {
  const receiver = flag ? array : other;
  return receiver.length;
}
total = 0;
for (let i = 0; i < 16000; i++) total += chooseReceiver(true, [1, 2, 3], null);
let gets = 0;
const ordinary = { get length() { gets++; return 17; } };
const proxy = new Proxy([1, 2], {
  get(target, key, receiver) {
    if (key === 'length') { gets++; return 23; }
    return Reflect.get(target, key, receiver);
  }
});
results.push(total, chooseReceiver(false, [], ordinary),
  chooseReceiver(true, proxy, null), gets, Array.isArray(proxy));

function replaceReceiver(array, replacement) {
  const before = array.length;
  array = replacement;
  return before + array.length;
}
total = 0;
for (let i = 0; i < 16000; i++) total += replaceReceiver([1, 2], [1, 2, 3]);
results.push(total, replaceReceiver([1, 2], ordinary), gets);
console.log(JSON.stringify(results));
