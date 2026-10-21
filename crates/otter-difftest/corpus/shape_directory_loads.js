// Complete shape directories must work across layouts, branches and mutations.
function make(index) {
  const object = {};
  for (let i = 0; i < index; i++) object['before' + i] = i;
  object.x = index + 1;
  for (let i = 0; i < 12; i++) object['after' + i] = i;
  return object;
}
const objects = [];
for (let i = 0; i < 24; i++) objects.push(make(i));
function read(object) {
  const a = object.x;
  object.other = 2;
  return a + object.x;
}
let sum = 0;
for (let i = 0; i < 18000; i++) sum += read(objects[i % objects.length]);
let gets = 0;
Object.defineProperty(objects[0], 'x', {get() { gets++; return 91; }, configurable: true});
const accessor = read(objects[0]);
const prototype = { x: 37 };
delete objects[1].x;
Object.setPrototypeOf(objects[1], prototype);
const inherited = read(objects[1]);
objects[2][Symbol('benign')] = true;
const symbol = read(objects[2]);
Object.defineProperty(objects[3], 'x', {value: 71, writable: false});
const frozen = read(objects[3]);
function mutate(object) { object.x = 103; }
function aroundCall(object) { const a = object.x; mutate(object); return a + object.x; }
let callSum = 0;
for (let i = 0; i < 12000; i++) callSum += aroundCall(objects[6 + i % 18]);
const changed = aroundCall(objects[4]);
let unusedGets = 0;
const exotic = new Proxy({}, {get(target, name) { if (name === 'x') unusedGets++; return 4; }});
function ignored(object) { object.x; }
for (let i = 0; i < 6000; i++) ignored(objects[6 + i % 18]);
ignored(exotic);
let threw = false;
try { ignored(null); } catch (e) { threw = e instanceof TypeError; }
// The child appends to a shared directory; the shorter parent's prefix must
// never expose the child's key, even if the hash bucket is already present.
const parent = make(8), child = make(8);
child.childOnly = 100;
function prefix(object) { return object.childOnly; }
for (let i = 0; i < 6000; i++) prefix(child);
const hidden = prefix(parent);
function zeroTrip(record, count) {
  let total = 0;
  for (let i = 0; i < count; i++) total += record.x;
  return total;
}
for (let i = 0; i < 6000; i++) zeroTrip(objects[6 + i % 18], 3);
const empty = zeroTrip(null, 0);
console.log(sum, accessor, gets, inherited, symbol, frozen, changed, callSum, unusedGets, threw, hidden, empty);
