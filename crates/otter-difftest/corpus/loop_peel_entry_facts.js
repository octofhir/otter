// Loops that call out, built with their first iteration ahead of them: the
// receiver proved there, a receiver whose shape changes inside the loop, a
// long-running loop entered through OSR, early exits, and number values that
// stay unboxed across the loop's entry and exit.
const results = [];

function Plan() { this.items = []; this.count = 0; }
Plan.prototype.size = function () { return this.items.length; };
Plan.prototype.at = function (i) { return this.items[i]; };
Plan.prototype.add = function (x) { this.items.push(x); this.count++; };
function Step(v) { this.v = v; }
Step.prototype.run = function (acc) { return acc + this.v; };
function Twice(v) { this.v = v; }
Twice.prototype.run = function (acc) { return acc + 2 * this.v; };

// `this` first read inside a loop that calls out on every iteration.
Plan.prototype.execute = function () {
  let acc = 0;
  for (let i = 0; i < this.size(); i++) acc = this.at(i).run(acc);
  return acc;
};
const plan = new Plan();
for (let i = 0; i < 40; i++) plan.add(i % 3 ? new Step(i) : new Twice(i));
let executed = 0;
for (let r = 0; r < 3000; r++) executed += plan.execute();
results.push(executed);

// The receiver's shape changes inside the loop, after its first iteration.
Plan.prototype.grow = function (n) {
  let total = 0;
  for (let i = 0; i < n; i++) {
    total += this.size() + this.count;
    if (i === 5) this.extra = i;           // transition mid-loop
    if (i === 9) delete this.extra;        // and again
    this.count = this.count + 1;
  }
  return total + (this.extra === undefined ? 1 : 0);
};
let grown = 0;
for (let r = 0; r < 2000; r++) {
  const p = new Plan();
  p.add(1);
  grown += p.grow(r % 13);
}
results.push(grown);

// Zero-trip, one-trip and early-exit loops whose exit merges an unboxed
// carry with the value it had before the loop.
function carry(xs, c, n) {
  for (let i = 0; i < n; i++) {
    if (xs.at(i) === undefined) break;
    c = (c * 3 + xs.at(i).v) & 0xffff;
  }
  return c;
}
let carried = 0;
for (let r = 0; r < 20000; r++) carried += carry(plan, r & 7, r % 4);
results.push(carried, carry(plan, 5, 0), carry(plan, 5, 100));

// One long call of a function whose loop is entered while it runs.
function longRun(p, n) {
  let s = 0.5;
  for (let i = 0; i < n; i++) s += p.at(i % p.size()).run(1) * 0.25;
  return s;
}
results.push(longRun(plan, 400000));

// An invariant number converted every iteration of a loop without calls.
function scale(arr, k, n) {
  let s = 0;
  for (let i = 0; i < n; i++) s += arr[i % arr.length] * k;
  return s;
}
const nums = [1, 2, 3, 4, 5, 6, 7];
let scaled = 0;
for (let r = 0; r < 3000; r++) scaled += scale(nums, r & 3, 64);
results.push(scaled, scale(nums, 0.5, 10), scale(nums, '2', 3));

console.log(JSON.stringify(results));
