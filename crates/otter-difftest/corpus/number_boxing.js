// Optimized Double words preserve Number semantics across integral values,
// signed zero, NaN, map keys, fields, arguments and array elements.
let calls = 0;
function divideBox(x) { calls++; return x / 2; }
function negateBox(x) { calls++; return -x; }
function mergeBox(x, flag) {
  let value = 0.5;
  if (flag) value = x * 3 - 1;
  return value;
}
function storeBox(target, x) {
  target.a = x - 0.25;
  target.b = x * x;
  return target;
}
function describe(x) {
  if (Object.is(x, -0)) return '-0';
  if (Number.isNaN(x)) return 'NaN';
  return String(x);
}
let sink = 0;
const target = { a: 0, b: 0 };
const cells = [];
for (let i = 0; i < 30000; i++) {
  sink += divideBox((i & 31) + 0.5);
  sink += negateBox((i & 31) + 0.25);
  sink += mergeBox((i & 15) + 0.5, i & 1);
  sink += storeBox(target, (i & 7) + 0.5).b;
  cells[i & 63] = divideBox(i);
}
const values = [0, -0, 1, -1, 0.5, -0.5, 3, -3, 6, -6, 2147483647, -2147483648,
  4294967294, -4294967296, -4294967298, 4294967296, 4294967298, 8589934592,
  -8589934592, 9007199254740992, 1e300, Infinity, -Infinity, NaN,
  Number.MIN_VALUE, -Number.MIN_VALUE, 1e-310, Number.MAX_VALUE, -Number.MAX_VALUE];
const results = [];
const keys = new Map();
for (const x of values) {
  const before = calls;
  const divided = divideBox(x);
  const negated = negateBox(x);
  if (calls !== before + 2) throw Error('replayed numeric call');
  if (!Object.is(divided, x / 2) || !Object.is(negated, -x)) throw Error('wrong Number');
  keys.set(divided, 'boxed');
  if (keys.get(x / 2) !== 'boxed') throw Error('wrong key identity');
  const merged = mergeBox(x, 1);
  if (!Object.is(merged, x * 3 - 1)) throw Error('wrong merge');
  storeBox(target, x);
  if (!Object.is(target.a, x - 0.25) || !Object.is(target.b, x * x)) throw Error('wrong field');
  cells[0] = negated;
  if (!Object.is(cells[0], -x)) throw Error('wrong element');
  results.push(describe(divided), describe(negated), describe(merged), describe(target.b));
}
results.push(keys.size, keys.has(0), keys.has(-0), keys.get(1073741823.5));
let coercions = 0;
const object = { valueOf() { coercions++; return 7; } };
let before = calls;
if (divideBox(object) !== 3.5 || calls !== before + 1 || coercions !== 1) throw Error('coercion replay');
before = calls;
let threw = false;
try { divideBox(Symbol('number')); } catch (e) { threw = e instanceof TypeError; }
if (!threw || calls !== before + 1) throw Error('abrupt replay');
console.log('number-boxing', calls, coercions, sink, cells.length, results.join(','));
