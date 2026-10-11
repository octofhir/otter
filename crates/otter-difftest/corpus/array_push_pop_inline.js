// Array push and pop at hot method sites: inline appends and removals, growth,
// holes, numeric storage, young values in retained arrays, and every receiver
// state that leaves the inline path.
const results = [];

// A collection whose add and removeLast are inlined into their callers.
function Collection() { this.elms = new Array(); }
Collection.prototype.add = function (value) { this.elms.push(value); };
Collection.prototype.removeLast = function () { return this.elms.pop(); };
Collection.prototype.size = function () { return this.elms.length; };

let total = 0;
for (let round = 0; round < 3000; round++) {
  const c = new Collection();
  for (let i = 0; i < (round % 23); i++) c.add({ i, round });
  while (c.size() > 0) total += c.removeLast().i;
  total += c.removeLast() === undefined ? 1 : 0;
}
results.push(total);

// Push results and lengths through one site; pop of every value.
function pushAll(array, count) {
  let lengths = 0;
  for (let i = 0; i < count; i++) lengths += array.push(i * 3);
  return lengths;
}
function popAll(array) {
  let sum = 0;
  while (array.length) sum += array.pop();
  return sum + (array.pop() === undefined ? 1000 : 0);
}
for (let round = 0; round < 200; round++) {
  const array = [];
  const lengths = pushAll(array, 50 + round % 7);
  if (round % 50 === 0) results.push(lengths, popAll(array), array.length);
  else popAll(array);
}

// Young values pushed into retained arrays survive later collections.
const retained = [];
for (let i = 0; i < 64; i++) retained.push([]);
for (let round = 0; round < 40; round++) {
  for (let i = 0; i < retained.length; i++) retained[i].push({ round, tag: 'r' + round + ':' + i });
  let garbage = 0;
  for (let j = 0; j < 4000; j++) garbage += { j }.j & 1;
  if (garbage < 0) results.push('never');
}
let retainedSum = 0;
for (const list of retained) {
  for (const entry of list) retainedSum += entry.round + entry.tag.length;
  retainedSum += list.pop().round;
}
results.push(retainedSum);

// A hole at the end reads through the prototype; a numeric array keeps its kind.
function popOne(array) { return array.pop(); }
function pushOne(array, value) { return array.push(value); }
for (let i = 0; i < 5000; i++) popOne(pushOne([i], i) ? [i, i + 1] : []);
Array.prototype[1] = 'inherited';
const holey = [0, , ];
results.push(holey.length, popOne(holey), holey.length);
delete Array.prototype[1];
const doubles = [1.5, 2.5];
results.push(pushOne(doubles, 3.5), popOne(doubles), popOne(doubles), doubles.join());
const mixed = [1.5];
results.push(pushOne(mixed, 'tagged'), mixed.join(), popOne(mixed), popOne(mixed));

// A length past the dense prefix, a sealed array, a frozen length.
const tail = [1, 2];
tail.length = 5;
results.push(pushOne(tail, 9), tail.length, tail[5], 3 in tail, popOne(tail), tail.length);
const sealed = Object.seal([1, 2]);
try { pushOne(sealed, 3); results.push('pushed'); } catch (e) { results.push(e.constructor.name, sealed.length); }
const fixed = [1, 2, 3];
Object.defineProperty(fixed, 'length', { writable: false });
try { popOne(fixed); results.push('popped'); } catch (e) { results.push(e.constructor.name, fixed.length); }

// A non-array receiver and a replaced builtin at the same sites.
const likeArray = { length: 0, push: Array.prototype.push, pop: Array.prototype.pop };
results.push(pushOne(likeArray, 'x'), likeArray.length, popOne(likeArray), likeArray.length);
const subclassed = new (class extends Array {})();
results.push(pushOne(subclassed, 4), popOne(subclassed), subclassed.length);
const originalPush = Array.prototype.push;
Array.prototype.push = function (value) { return originalPush.call(this, value, value); };
const doubled = [];
results.push(pushOne(doubled, 7), doubled.join());
Array.prototype.push = originalPush;
results.push(pushOne(doubled, 8), doubled.join());

// An indexed setter anywhere in the realm makes an append observable.
let setterCalls = 0;
Object.defineProperty(Object.prototype, '3', {
  configurable: true, set(value) { setterCalls++; }, get() { return 'getter'; },
});
const observed = [0, 1, 2];
results.push(pushOne(observed, 3), setterCalls, observed.length, Object.prototype.hasOwnProperty.call(observed, 3));
delete Object.prototype[3];
results.push(pushOne(observed, 4), observed.join());

console.log(JSON.stringify(results));
