// `length` sites that have read only Arrays: lengths that change between
// reads, a site that later reads a string, a typed array, an arguments object
// or an ordinary object, and an Array whose length leaves the int32 range.
const results = [];

function Bag() { this.items = []; }
Bag.prototype.size = function () { return this.items.length; };
Bag.prototype.add = function (x) { this.items.push(x); };
function fill(bag, n) {
  let seen = 0;
  for (let i = 0; i < n; i++) {
    bag.add(i);
    seen += bag.size();      // grows every iteration
  }
  while (bag.size() > n / 2) { bag.items.pop(); seen -= 1; }
  return seen + bag.size();
}
let filled = 0;
for (let r = 0; r < 2000; r++) filled += fill(new Bag(), 20 + (r & 7));
results.push(filled);

// One site, Arrays while hot, then every other kind of receiver.
function len(x) { return x.length; }
let total = 0;
for (let i = 0; i < 50000; i++) total += len([i, i, i].slice(i & 1));
results.push(total);
results.push(len('string'), len(new Uint8Array(7)), len({ length: 11 }),
  (function () { return len(arguments); })(1, 2, 3), len(function (a, b) {}));
let mixed = 0;
for (let i = 0; i < 30000; i++) mixed += len(i % 3 === 0 ? 'abc' : [i]);
results.push(mixed);

// Lengths past the int32 range.
const huge = [];
huge.length = 2 ** 31 + 5;
results.push(len(huge), len([].concat([1, 2])));
const big = [];
big[2 ** 32 - 2] = 1;
results.push(len(big));

// A length read inside a loop that also shrinks the Array.
function drain(arr) {
  let steps = 0;
  for (let i = 0; i < arr.length; i++) { arr.length = arr.length - 1; steps++; }
  return steps * 1000 + arr.length;
}
let drained = 0;
for (let r = 0; r < 3000; r++) drained += drain(new Array(30 + (r & 15)).fill(0));
results.push(drained);

// One long call whose inner loop is entered through OSR while it reads a
// global Array's length and elements.
var globalItems = [1, 2, 3, 4, 5];
function nested(rounds) {
  let s = 0;
  for (let r = 0; r < rounds; r++) {
    for (let i = 0; i < globalItems.length; i++) s += globalItems[i];
  }
  return s;
}
results.push(nested(300000));

console.log(JSON.stringify(results));
