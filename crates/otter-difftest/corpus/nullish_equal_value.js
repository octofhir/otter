// Nullish equality as a value preserves effects and handles every value kind.
let conversions = 0;
const opaque = {
  [Symbol.toPrimitive]() {
    conversions++;
    throw new Error('unexpected conversion');
  }
};
const proxy = new Proxy({}, {
  get() {
    conversions++;
    throw new Error('unexpected property read');
  }
});
function compare(value) {
  const equal = value == null;
  const reverse = undefined == value;
  const unequal = null != value;
  return [equal, reverse, unequal, value != undefined, value === null, value === undefined];
}
const values = [null, undefined, false, true, 0, -0, NaN, 1n, Symbol('s'), '', 'x', [], {}, opaque, proxy, function() {}, Math.abs, Boolean];
for (let i = 0; i < 8000; i++) compare(values[i % values.length]);
const result = values.map(compare);
console.log(JSON.stringify(result));
console.log(conversions);
