// ECMAScript ToInt32 / ToUint32 of doubles in optimized loops: fractions,
// negative zero, the int32 and uint32 boundaries, magnitudes past 2^53 and
// 2^63, the largest finite doubles, infinities and NaN. Values arrive from a
// double array so the bitwise operators see unboxed Float64 operands.
const inputs = [
  0.5, -0.5, -0, 1.9, -1.9, 2147483647.5, 2147483648, -2147483648.7,
  -2147483649, 4294967295.9, 4294967296, 4294967301.25, -4294967297,
  9007199254740993, 2 ** 53 + 2, 1e20, -1e20, 2 ** 62 + 2 ** 40,
  2 ** 63, -(2 ** 63), 2 ** 63 + 2 ** 11 * 3, 2 ** 64 + 2 ** 12, 2 ** 84 + 2 ** 33,
  2 ** 85, 1.7976931348623157e308, -1.7976931348623157e308, 5e-324,
  Infinity, -Infinity, NaN, 123456789012.75, -98765432109.25,
];
function fold(values) {
  let acc = 0;
  let unsigned = 0;
  for (let i = 0; i < values.length; i++) {
    const v = values[i];
    acc = (acc * 31 + (v | 0)) | 0;
    acc = (acc ^ (v >> 3)) | 0;
    unsigned = (unsigned + (v >>> 0)) % 4294967296;
    acc = (acc + (v & 0xffff)) | 0;
  }
  return [acc, unsigned];
}
function each(values) {
  const out = [];
  for (let i = 0; i < values.length; i++) out.push(values[i] | 0, values[i] >>> 0);
  return out;
}
let checksum = 0;
for (let round = 0; round < 3000; round++) {
  const [acc, unsigned] = fold(inputs);
  checksum = (checksum + acc + unsigned) | 0;
}
console.log(JSON.stringify([checksum, fold(inputs), each(inputs)]));
