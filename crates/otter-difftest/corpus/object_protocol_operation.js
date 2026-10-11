// Explicit operation identity survives coercion, reentry and committed throws.
// Caller objects remain live while warmed leaf operations enter allocating hooks.
const out = [];
const marker = { tag: 'operation-throw' };
const counts = {};
const names = ['key', 'lookup', 'has', 'targetGet', 'instanceLookup', 'method',
  'candidateGet', 'truthy', 'eq', 'left', 'right', 'protoGet', 'protoSet', 'finally'];
let late = false;
let throwing = '';
let trace = [];
function reset(throwAt) {
  for (const name of names) counts[name] = 0;
  throwing = throwAt || '';
  trace = [];
}
function churn(value) {
  let head = { value, previous: null };
  for (let i = 0; i < 48; i++) head = { value, previous: head };
  return head.previous.value;
}
function effect(name) {
  counts[name]++;
  if (late) {
    trace.push(name);
    churn(marker);
    if (throwing === name) throw marker;
  }
}
function check(name, actual, expected) {
  const text = JSON.stringify(actual);
  if (text !== JSON.stringify(expected)) throw new Error(name + ': ' + text);
  out.push([name, actual]);
}
function snapshot(keys) { return keys.map(key => counts[key]); }

function hasOp(key, object) { return key in object; }
function eqOp(a, b) { return a == b; }
function neOp(a, b) { return a != b; }
function instanceOp(value, target) { return value instanceof target; }
function hasMiddle(a, b) { return hasOp(a, b); }
function eqMiddle(a, b) { return eqOp(a, b); }
function neMiddle(a, b) { return neOp(a, b); }
function instanceMiddle(a, b) { return instanceOp(a, b); }
function outer(kind, a, b) {
  const retained = { text: 'caller', child: { code: 37 } };
  const alias = retained;
  try {
    let value;
    if (kind === 'has') value = hasMiddle(a, b);
    else if (kind === 'eq') value = eqMiddle(a, b);
    else if (kind === 'ne') value = neMiddle(a, b);
    else value = instanceMiddle(a, b);
    return ['ok', value, retained === alias, retained.text, retained.child.code];
  } catch (error) {
    return ['throw', error === marker, error instanceof TypeError,
      retained === alias, retained.text, retained.child.code];
  } finally { counts.finally++; }
}
const okTrue = ['ok', true, true, 'caller', 37];
const okFalse = ['ok', false, true, 'caller', 37];
const caught = ['throw', true, false, true, 'caller', 37];

const symbolKey = Symbol('present');
const target = { [symbolKey]: 11 };
Object.defineProperty(target, 'yes', { get() { effect('targetGet'); return 13; } });
function hasTrap(object, key) { effect('has'); return Reflect.has(object, key); }
const proxy = new Proxy(target, { get has() { effect('lookup'); return hasTrap; } });
const key = { [Symbol.toPrimitive](hint) {
  if (hint !== 'string') throw new Error('key hint');
  effect('key');
  return 'yes';
} };
const equalObject = { valueOf() { effect('eq'); return 7; } };

function InstanceTarget() {}
const truthy = { valueOf() { effect('truthy'); throw marker; } };
let truthyResult = false;
function hasInstance(value) {
  effect('method');
  if (this !== InstanceTarget) throw new Error('hasInstance receiver');
  return truthyResult ? truthy : value.marker === 37;
}
Object.defineProperty(InstanceTarget, Symbol.hasInstance, {
  get() { effect('instanceLookup'); return hasInstance; }
});
const boundTarget = InstanceTarget.bind(null);
const candidate = new Proxy({ marker: 37 }, {
  get(object, name) {
    if (name === 'marker') effect('candidateGet');
    return Reflect.get(object, name);
  }
});

reset();
for (let i = 0; i < 10000; i++) {
  outer('has', key, proxy);
  outer('eq', equalObject, 7);
  outer('ne', equalObject, 7);
  outer('instance', candidate, InstanceTarget);
  instanceOp(candidate, boundTarget);
}
late = true;
const hasCounts = ['key', 'lookup', 'has', 'targetGet', 'finally'];
reset();
check('has', [outer('has', key, proxy), snapshot(hasCounts), trace],
  [okTrue, [1, 1, 1, 0, 1], ['key', 'lookup', 'has']]);
for (const stage of ['key', 'lookup', 'has']) {
  reset(stage);
  const expected = stage === 'key' ? [1, 0, 0, 0, 1]
    : stage === 'lookup' ? [1, 1, 0, 0, 1] : [1, 1, 1, 0, 1];
  check('has-throw-' + stage, [outer('has', key, proxy), snapshot(hasCounts)], [caught, expected]);
}
reset();
check('has-symbol', [outer('has', symbolKey, proxy), snapshot(hasCounts)],
  [okTrue, [0, 1, 1, 0, 1]]);
reset('key');
check('has-primitive', [outer('has', key, 3), snapshot(hasCounts)],
  [['throw', false, true, true, 'caller', 37], [0, 0, 0, 0, 1]]);

const instanceCounts = ['instanceLookup', 'method', 'candidateGet', 'truthy', 'finally'];
for (const type of [InstanceTarget, boundTarget]) {
  reset();
  check('instance', [outer('instance', candidate, type), snapshot(instanceCounts), trace],
    [okTrue, [1, 1, 1, 0, 1], ['instanceLookup', 'method', 'candidateGet']]);
}
truthyResult = true;
reset();
check('instance-truthy', [outer('instance', candidate, InstanceTarget), snapshot(instanceCounts)],
  [okTrue, [1, 1, 0, 0, 1]]);
truthyResult = false;
for (const stage of ['instanceLookup', 'method', 'candidateGet']) {
  reset(stage);
  const expected = stage === 'instanceLookup' ? [1, 0, 0, 0, 1]
    : stage === 'method' ? [1, 1, 0, 0, 1] : [1, 1, 1, 0, 1];
  check('instance-throw-' + stage,
    [outer('instance', candidate, InstanceTarget), snapshot(instanceCounts)], [caught, expected]);
}

for (const kind of ['eq', 'ne']) {
  for (const number of [7, 8]) {
    reset();
    const value = (number === 7) === (kind === 'eq');
    check(kind + '-' + number, [outer(kind, equalObject, number), counts.eq, counts.finally],
      [value ? okTrue : okFalse, 1, 1]);
  }
  reset('eq');
  check(kind + '-throw', [outer(kind, equalObject, 7), counts.eq, counts.finally], [caught, 1, 1]);
}
function nullEq(value) { return value == null; }
function undefinedNe(value) { return value != undefined; }
late = false;
for (let i = 0; i < 10000; i++) { nullEq(equalObject); undefinedNe(equalObject); }
late = true;
reset('eq');
check('nullish', [eqOp(undefined, null), neOp(undefined, null), nullEq(equalObject),
  undefinedNe(equalObject), nullEq(undefined), undefinedNe(null), counts.eq],
  [true, false, false, true, true, false, 0]);

function add(a, b) { return a + b; }
function sub(a, b) { return a - b; }
function mul(a, b) { return a * b; }
function div(a, b) { return a / b; }
function rem(a, b) { return a % b; }
function pow(a, b) { return a ** b; }
function lt(a, b) { return a < b; }
function le(a, b) { return a <= b; }
function gt(a, b) { return a > b; }
function ge(a, b) { return a >= b; }
function and(a, b) { return a & b; }
function or(a, b) { return a | b; }
function xor(a, b) { return a ^ b; }
function shl(a, b) { return a << b; }
function shr(a, b) { return a >> b; }
function ushr(a, b) { return a >>> b; }
const binary = [add, sub, mul, div, rem, pow, lt, le, gt, ge, and, or, xor, shl, shr, ushr];
const expectedBinary = [5, 1, 6, 1.5, 1, 9, false, false, true, true, 2, 3, 1, 12, 0, 0];
let leftValue = 3;
const left = { valueOf() { effect('left'); return leftValue; } };
const right = { valueOf() { effect('right'); return 2; } };
function binaryOuter(fn, a, b) {
  const retained = { text: 'binary', child: { code: 41 } };
  try { return ['ok', fn(a, b), retained.text, retained.child.code]; }
  catch (error) { return ['throw', error === marker, retained.text, retained.child.code]; }
}
late = false;
reset();
for (let i = 0; i < 10000; i++) {
  for (const fn of binary) fn(left, right);
  binaryOuter(add, left, right);
}
late = true;
for (let i = 0; i < binary.length; i++) {
  reset();
  check('binary-' + i, [binaryOuter(binary[i], left, right), snapshot(['left', 'right']), trace],
    [['ok', expectedBinary[i], 'binary', 41], [1, 1], ['left', 'right']]);
}
leftValue = 'v';
reset();
check('string-add', [binaryOuter(add, left, right), trace], [['ok', 'v2', 'binary', 41], ['left', 'right']]);
leftValue = 3;
for (const stage of ['left', 'right']) {
  reset(stage);
  check('binary-throw-' + stage,
    [binaryOuter(add, left, right), snapshot(['left', 'right']), trace],
    [['throw', true, 'binary', 41], stage === 'left' ? [1, 0] : [1, 1],
      stage === 'left' ? ['left'] : ['left', 'right']]);
}

const prototype = {
  get value() { effect('protoGet'); return this.own; },
  set value(value) { effect('protoSet'); this.own = value; }
};
const home = { __proto__: prototype, own: 43,
  read() { return super.value; },
  write(value) { super.value = value; return this.own; }
};
function literal(prototype, value) { return { __proto__: prototype, own: value }; }
function link(base) { return class Linked extends base {}; }
class Base { read() { return this.own; } }
late = false;
reset();
for (let i = 0; i < 10000; i++) { home.read(); home.write(43); literal(prototype, 47); link(Base); }
late = true;
reset();
const linked = link(Base);
const instance = new linked();
instance.own = 53;
const shaped = literal(prototype, 47);
check('prototypes', [home.read(), home.write(59), shaped.value, instance.read(),
  Object.getPrototypeOf(shaped) === prototype, Object.getPrototypeOf(linked) === Base,
  instance instanceof Base, snapshot(['protoGet', 'protoSet'])],
  [43, 59, 47, 53, true, true, true, [2, 1]]);
for (const stage of ['protoGet', 'protoSet']) {
  reset(stage);
  let caughtMarker = false;
  const retained = { code: 61 };
  try { if (stage === 'protoGet') home.read(); else home.write(67); }
  catch (error) { caughtMarker = error === marker; }
  check('super-throw-' + stage,
    [caughtMarker, retained.code, home.own, snapshot(['protoGet', 'protoSet'])],
    [true, 61, 59, stage === 'protoGet' ? [1, 0] : [0, 1]]);
}
console.log(JSON.stringify(out));
