// Keyed stores at an array's length append: hot loops filling fresh arrays
// with objects, closures and numbers, gaps past the length, arrays that
// turn holey, frozen or sealed arrays, an indexed setter on the prototype,
// and numeric storage appended to, popped, truncated, grown past its
// capacity and turned tagged by a later value must answer exactly what the
// interpreter answers. Generic sites that also see plain objects append
// past a full storage, read holes and indices past the length, and read
// through an array whose own prototype supplies the indices.
'use strict';
function fill(count, make) {
  const out = [];
  for (let i = 0; i < count; i++) out[i] = make(i);
  return out;
}
function fillGap(count) {
  const out = [];
  for (let i = 0; i < count; i++) out[i * 2] = i;
  return out;
}
function appendTo(target, count, value) {
  for (let i = 0; i < count; i++) target[target.length] = value + i;
  return target;
}
let checksum = 0;
const log = [];
for (let round = 0; round < 60; round++) {
  const objects = fill(300 + round, (i) => ({ i }));
  const closures = fill(200, (i) => () => i * 3);
  const numbers = fill(250, (i) => i * 0.5);
  const gaps = fillGap(40);
  checksum = (checksum + objects[objects.length - 1].i + closures[199]() + numbers[249] +
    gaps.length + (5 in gaps ? 1 : 0)) % 1000003;
  const mixed = appendTo([1, 'two'], 100, round);
  checksum = (checksum + mixed.length + mixed[101]) % 1000003;
  // A young value stored into an array that has grown old.
  const kept = appendTo([], 1000, 0);
  for (let i = 0; i < 50; i++) kept[kept.length] = { round, i };
  checksum = (checksum + kept[kept.length - 1].i + kept.length) % 1000003;
}
// Generic keyed sites: arrays and plain objects through the same access.
function genericGet(container, key) { return container[key]; }
function genericPut(container, key, value) { container[key] = value; }
const inherited = Object.create(Array.prototype);
inherited[3] = 'inherited';
for (let round = 0; round < 40; round++) {
  const grown = [];
  const bag = {};
  for (let i = 0; i < 70; i++) {
    genericPut(grown, i, { i, round });
    genericPut(bag, 'k' + i, i);
  }
  const holey = [1, , 3, , 5];
  holey.length = 9;
  let absent = 0;
  for (let i = 0; i < 12; i++) {
    if (genericGet(holey, i) === undefined) absent++;
    if (genericGet(bag, 'k' + i) === undefined) absent += 100;
  }
  const shadowed = [0, , 2, , 4];
  Object.setPrototypeOf(shadowed, inherited);
  checksum = (checksum + grown.length + grown[69].i + absent +
    (genericGet(shadowed, 3) === 'inherited' ? 7 : 0) + (genericGet(shadowed, 1) === undefined ? 1 : 0)) % 1000003;
}
const frozen = Object.freeze([1, 2, 3]);
try { appendTo(frozen, 1, 0); log.push('frozen appended'); } catch (e) { log.push(e.constructor.name); }
const sealed = Object.seal([1, 2, 3]);
try { appendTo(sealed, 1, 0); log.push('sealed appended'); } catch (e) { log.push(e.constructor.name); }
const fixedLength = [1, 2, 3];
Object.defineProperty(fixedLength, 'length', { writable: false });
try { appendTo(fixedLength, 1, 0); log.push('fixed appended'); } catch (e) { log.push(e.constructor.name); }
let setterHits = 0;
Object.defineProperty(Array.prototype, 7, { set(v) { setterHits += v; }, configurable: true });
const viaSetter = fill(10, (i) => i);
log.push(viaSetter.length, 7 in viaSetter, setterHits);
delete Array.prototype[7];
const after = fill(10, (i) => i);
log.push(after.length, after[7]);
function numeric(rounds) {
  let sum = 0;
  for (let r = 0; r < rounds; r++) {
    const a = [];
    for (let i = 0; i < 40; i++) a[i] = i * 0.5;
    a.pop();
    a[a.length] = -1;
    a.length = 10;
    a[a.length] = 2.25;
    if (r % 97 === 0) a[a.length] = 'tag';
    const holey = [1.5, , 3.5];
    holey[holey.length] = 4.5;
    sum += a.length + a[10] + (typeof a[11] === 'string' ? 100 : 0) + holey.length + (1 in holey ? 1 : 0);
  }
  return sum;
}
log.push(numeric(3000));
console.log(checksum, JSON.stringify(log));
