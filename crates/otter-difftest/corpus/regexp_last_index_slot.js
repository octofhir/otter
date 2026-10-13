// `lastIndex` reads and writes at hot sites: RegExps of every flag kind, a
// subclass instance, values of every type (cells written into old RegExps
// across collections), a non-writable `lastIndex` in sloppy and strict code,
// a frozen RegExp, and ordinary objects owning `lastIndex` at the same sites.
const results = [];

function setLast(re, value) {
  re.lastIndex = value;
}

function getLast(re) {
  return re.lastIndex;
}

function setLastStrict(re, value) {
  'use strict';
  re.lastIndex = value;
}

class Sub extends RegExp {}
const kinds = [/a/, /a/g, /a/y, /a/gy, new RegExp('b', 'g'), new Sub('c', 'g')];

let sum = 0;
for (let i = 0; i < 20000; i++) {
  const re = kinds[i % kinds.length];
  setLast(re, i % 7);
  sum += getLast(re);
  if (re.global) {
    setLast(re, 0);
    re.exec('xaxbxc');
    sum += getLast(re);
  }
}
results.push(sum);

// Values of every type round-trip exactly; cells survive collections while
// stored in RegExps allocated long before.
const holders = [];
for (let i = 0; i < 64; i++) holders.push(new RegExp('h' + i, 'g'));
let churn = [];
for (let round = 0; round < 40; round++) {
  for (let i = 0; i < holders.length; i++) {
    const value =
      i % 4 === 0 ? { round, i } : i % 4 === 1 ? 'r' + round + '_' + i : i % 4 === 2 ? round + i / 8 : [round, i];
    setLast(holders[i], value);
  }
  for (let j = 0; j < 2000; j++) churn.push({ j, s: 'x' + j });
  if (churn.length > 20000) churn = [];
}
let shape = 0;
for (let i = 0; i < holders.length; i++) {
  const value = getLast(holders[i]);
  if (i % 4 === 0) shape += value.round * 1000 + value.i;
  else if (i % 4 === 1) shape += value.length;
  else if (i % 4 === 2) shape += value;
  else shape += value[0] + value[1];
}
results.push(shape);

// A non-writable lastIndex: sloppy writes are ignored, strict writes throw.
const pinned = /p/g;
Object.defineProperty(pinned, 'lastIndex', { value: 3, writable: false });
let ignored = 0;
let thrown = 0;
for (let i = 0; i < 5000; i++) {
  const re = i % 5 === 4 ? pinned : kinds[i % kinds.length];
  setLast(re, 9);
  ignored += getLast(re);
  try {
    setLastStrict(re, 11);
  } catch (e) {
    if (e instanceof TypeError) thrown++;
  }
}
results.push(ignored, thrown, getLast(pinned));

// A frozen RegExp keeps its lastIndex.
const frozen = Object.freeze(/f/g);
setLast(frozen, 5);
let frozenThrew = false;
try {
  setLastStrict(frozen, 6);
} catch (e) {
  frozenThrew = e instanceof TypeError;
}
results.push(getLast(frozen), frozenThrew);

// Ordinary objects owning lastIndex share the sites with RegExps.
const plain = [{ lastIndex: 1 }, { lastIndex: 2, other: true }, Object.create({ lastIndex: 7 })];
let mixed = 0;
for (let i = 0; i < 9000; i++) {
  const target = i % 3 === 0 ? kinds[i % kinds.length] : plain[i % plain.length];
  setLast(target, i % 13);
  mixed += getLast(target);
}
results.push(mixed, Object.getPrototypeOf(plain[2]).lastIndex, plain[2].lastIndex);

console.log(JSON.stringify(results));
