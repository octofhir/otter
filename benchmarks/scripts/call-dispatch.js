// Direct calls, closures, method calls, and simple recursion.
function add3(a, b, c) {
  return (a + b + c) | 0;
}
function makeMul(k) {
  return function mul(x) {
    return (x * k) | 0;
  };
}
const obj = {
  bias: 7,
  mix(x, y) {
    return ((x ^ y) + this.bias) | 0;
  },
};
function smallRec(n) {
  return n <= 1 ? 1 : (smallRec(n - 1) + smallRec(n - 2)) | 0;
}

const mul3 = makeMul(3);
let acc = 0;
for (let r = 0; r < 120; r++) {
  for (let i = 0; i < 7000; i++) {
    acc = (acc + add3(i, r, acc & 15)) | 0;
    acc = (acc ^ mul3(i + r)) | 0;
    acc = (acc + obj.mix(i, acc)) | 0;
  }
  acc = (acc + smallRec(10)) | 0;
}
console.log(acc >>> 0);
