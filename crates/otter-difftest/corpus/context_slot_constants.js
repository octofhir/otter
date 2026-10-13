// Closure-context bindings hot code reads as constants, changed later from
// every kind of writer: cold interpreted code, a hot loop's own store, a
// sloppy direct eval, an aliased `arguments` element, and a shape change of
// a constant object. Functions are also instantiated over more than one
// context, so code specialized to one context meets another.
const results = [];

// Module-level constants read from inlined methods.
var STATE_HELD = 4;
var STATE_RUNNABLE = 1;
var SCALE = 2.5;
var FLAG = true;
var NOTHING = null;
var Direction = { NONE: 0, FORWARD: 1, BACKWARD: -1 };

function Task(state) {
  this.state = state;
  this.direction = (state & 1) ? 1 : -1;
}
Task.prototype.isHeld = function () {
  return (this.state & STATE_HELD) !== 0;
};
Task.prototype.isRunnable = function () {
  return (this.state & STATE_RUNNABLE) !== 0;
};
Task.prototype.weight = function () {
  return this.state * SCALE + (FLAG ? 1 : 10) + (NOTHING === null ? 2 : 300);
};
Task.prototype.forward = function () {
  return this.direction === Direction.FORWARD;
};

function run(tasks, rounds) {
  let held = 0;
  let runnable = 0;
  let weight = 0;
  let forward = 0;
  for (let r = 0; r < rounds; r++) {
    for (let i = 0; i < tasks.length; i++) {
      const t = tasks[i];
      if (t.isHeld()) held++;
      if (t.isRunnable()) runnable++;
      weight += t.weight();
      if (t.forward()) forward++;
    }
  }
  return [held, runnable, weight, forward].join(',');
}

const tasks = [];
for (let i = 0; i < 16; i++) tasks.push(new Task(i));
results.push(run(tasks, 3000));

// Cold interpreted writes after the hot code compiled.
STATE_HELD = 12;
SCALE = 1.5;
results.push(run(tasks, 3000));
FLAG = false;
NOTHING = undefined;
results.push(run(tasks, 3000));

// A shape change of a constant object.
Direction.UP = 5;
results.push(run(tasks, 3000));
Direction.FORWARD = 0;
results.push(run(tasks, 3000));

// A hot loop that both reads and writes a watched binding.
var counter = 7;
Task.prototype.bump = function () {
  return this.state + counter;
};
function bumpAll(rounds) {
  let sum = 0;
  for (let r = 0; r < rounds; r++) {
    for (let i = 0; i < tasks.length; i++) sum += tasks[i].bump();
    if (r === rounds / 2) counter = 11;
  }
  return sum;
}
results.push(bumpAll(2000), bumpAll(2000));

// A sloppy direct eval writing a binding hot code holds.
var LIMIT = 10;
Task.prototype.limited = function () {
  return this.state < LIMIT;
};
function countLimited(rounds) {
  let n = 0;
  for (let r = 0; r < rounds; r++) for (let i = 0; i < tasks.length; i++) if (tasks[i].limited()) n++;
  return n;
}
results.push(countLimited(3000));
eval('LIMIT = 3');
results.push(countLimited(3000));

// An aliased `arguments` element writing a captured parameter: the
// sloppy function's mapped arguments object aliases the binding its inner
// closure reads.
function makeReader(base) {
  const args = arguments;
  return {
    read(t) {
      return t.state + base;
    },
    change(next) {
      args[0] = next;
    },
  };
}
const reader = makeReader(100);
Task.prototype.readWith = function (r) {
  return r.read(this);
};
function readAll(rounds) {
  let sum = 0;
  for (let k = 0; k < rounds; k++) for (let i = 0; i < tasks.length; i++) sum += tasks[i].readWith(reader);
  return sum;
}
results.push(readAll(3000));
reader.change(1000);
results.push(readAll(3000));

// The same function over many contexts: each closure sees its own binding.
function factory(offset) {
  return function (t) {
    return t.state * offset;
  };
}
const fns = [];
for (let i = 0; i < 5; i++) fns.push(factory(i + 1));
let factorySum = 0;
for (let r = 0; r < 4000; r++) {
  const f = fns[r < 3000 ? 0 : r % 5];
  for (let i = 0; i < tasks.length; i++) factorySum += f(tasks[i]);
}
results.push(factorySum);

// A binding two scopes out, and a scope with more bindings than a watch
// covers.
function outer() {
  var deep = 3;
  return function middle() {
    var a0 = 0, a1 = 1, a2 = 2, a3 = 3, a4 = 4, a5 = 5, a6 = 6, a7 = 7, a8 = 8, a9 = 9;
    var b0 = 0, b1 = 1, b2 = 2, b3 = 3, b4 = 4, b5 = 5, b6 = 6, b7 = 7, b8 = 8, b9 = 9;
    var c0 = 0, c1 = 1, c2 = 2, c3 = 3, c4 = 4, c5 = 5, c6 = 6, c7 = 7, c8 = 8, c9 = 9;
    var d0 = 0, d1 = 1, d2 = 2, d3 = 3, d4 = 4, d5 = 5, d6 = 6, d7 = 7, d8 = 8, d9 = 9;
    var e0 = 0, e1 = 1, e2 = 2, e3 = 3, e4 = 4, e5 = 5, e6 = 6, e7 = 7, e8 = 8, e9 = 9;
    var f0 = 0, f1 = 1, f2 = 2, f3 = 3, f4 = 4, f5 = 5, f6 = 6, f7 = 7, f8 = 8, f9 = 9;
    var g0 = 0, g1 = 1, g2 = 2, g3 = 3, g4 = 4, g5 = 5, g6 = 6, g7 = 7, g8 = 8, g9 = 9;
    var last = 70;
    const all = () => a9 + b9 + c9 + d9 + e9 + f9 + g9 + a0 + g0;
    return {
      inner(t) {
        return t.state * deep + last + all();
      },
      set(v) {
        deep = v;
        last = v * 10;
      },
    };
  };
}
const nested = outer()();
let nestedSum = 0;
for (let r = 0; r < 3000; r++) {
  if (r === 2000) nested.set(4);
  for (let i = 0; i < tasks.length; i++) nestedSum += nested.inner(tasks[i]);
}
results.push(nestedSum);

// Heap-backed primitives still need ordinary sloppy receiver boxing when
// a watched slot feeds Function.prototype.call and must preserve its receiver
// conversion through the generated call or body splice.
var primitiveText = 'watched';
var primitiveSymbol = Symbol('watched');
var primitiveBigInt = 123n;
var actualObject = { name: 'watched' };
function receiverType() {
  return typeof this + ':' + arguments.length;
}
function Reader() {}
Reader.prototype.text = function () {
  return receiverType.call(primitiveText);
};
Reader.prototype.symbol = function () {
  return receiverType.call(primitiveSymbol);
};
Reader.prototype.bigint = function () {
  return receiverType.call(primitiveBigInt);
};
Reader.prototype.object = function () {
  return receiverType.call(actualObject);
};
const boxedReader = new Reader();
const primitiveThisCounts = [0, 0, 0, 0];
for (let i = 0; i < 24000; i++) {
  primitiveThisCounts[0] += boxedReader.text() === 'object:0';
  primitiveThisCounts[1] += boxedReader.symbol() === 'object:0';
  primitiveThisCounts[2] += boxedReader.bigint() === 'object:0';
  primitiveThisCounts[3] += boxedReader.object() === 'object:0';
}
results.push(primitiveThisCounts);

console.log(JSON.stringify(results));
