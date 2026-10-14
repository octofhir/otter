// Append hits and growth share the same representation and observable effects.
const results = [];
function appendOne(array, value) { return array.push(value); }
function appendIndex(array, value) { array[array.length] = value; return array.length; }
const buffer = [0, 1, 2, 3];
buffer.pop();
let total = 0;
for (let i = 0; i < 16000; i++) {
  total += appendOne(buffer, i & 7);
  total += buffer.pop();
  total += appendIndex(buffer, i & 3);
  total += buffer.pop();
}
results.push(total, buffer.length);
buffer.named = 'expando';
results.push(appendOne(buffer, -0), Object.is(buffer[3], -0));
results.push(appendOne(buffer, NaN), Number.isNaN(buffer[4]));
const child = {marker: 'young-' + 731};
results.push(appendOne(buffer, child), buffer[5] === child, buffer.named);
delete buffer[1];
results.push(appendOne(buffer, true), 1 in buffer, buffer[6]);
const retained = [];
for (let i = 0; i < 256; i++) {
  const value = {marker: i, text: 'kept-' + i + 'x'.repeat(40)};
  appendOne(buffer, value);
  appendIndex(retained, value);
}
results.push(buffer.length, retained.length, buffer[7] === retained[0],
  buffer[262].marker, retained[127].text);

function attempt(array, value) {
  try { return appendOne(array, value); }
  catch (error) { return error.name; }
}
const readonly = [0, 1, 2];
Object.defineProperty(readonly, 'length', {writable: false});
results.push(attempt(readonly, 9), readonly.length, 3 in readonly);
const sealed = [0, 1, 2];
Object.preventExtensions(sealed);
results.push(attempt(sealed, 9), sealed.length, 3 in sealed);
const frozen = Object.freeze([0, 1, 2]);
results.push(attempt(frozen, 9), frozen.length, 3 in frozen);

let sets = 0, observed = 0;
const prototype = Object.create(Array.prototype);
Object.defineProperty(prototype, '3', {
  set(value) { sets++; observed = value.marker; }, configurable: true
});
const inherited = [0, 1, 2];
Object.setPrototypeOf(inherited, prototype);
results.push(appendOne(inherited, {marker: 817}), inherited.length,
  sets, observed, Object.hasOwn(inherited, '3'));
const sparse = [1];
sparse.length = 2097152;
results.push(appendOne(sparse, 71), sparse[2097152], 1 in sparse);
const holey = new Array(4);
holey[0] = 1.5;
results.push(appendOne(holey, 2.5), holey[4], 1 in holey);

let calls = 0;
const original = Array.prototype.push;
Array.prototype.push = function (value) {
  calls++;
  return Reflect.apply(original, this, [value + 10]);
};
const replaced = [0, 1, 2];
const replacedLength = appendOne(replaced, 7);
Array.prototype.push = original;
results.push(replacedLength, replaced[3], calls);
let proxySets = '';
const target = [0, 1, 2];
const proxy = new Proxy(target, {
  set(object, key, value, receiver) {
    proxySets += key + ',';
    return Reflect.set(object, key, value, receiver);
  }
});
results.push(appendOne(proxy, 9), target[3], proxySets);
console.log(JSON.stringify(results));
