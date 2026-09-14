function multiply(left, right) {
  return left * right;
}

for (let i = 0; i < 70000; i++) {
  multiply(3, 4);
}

let conversions = 0;
const coercive = {
  valueOf() {
    conversions++;
    return -1;
  },
};
let mixedBigInt;
try {
  multiply(1n, 1);
  mixedBigInt = "missing throw";
} catch (error) {
  mixedBigInt = error.name;
}

console.log(
  JSON.stringify([
    Object.is(multiply(1, 0), 0),
    Object.is(multiply(-1, 0), -0),
    Object.is(multiply(1, -0), -0),
    Object.is(multiply(-1, -0), 0),
    Object.is(multiply(0, -1), -0),
    Object.is(multiply(-0, 1), -0),
    Object.is(multiply(-0, -1), 0),
    Object.is(multiply(2147483647, 2), 4294967294),
    Object.is(multiply(Number.MAX_VALUE, 2), Infinity),
    Number.isNaN(multiply(NaN, 1)),
    Object.is(multiply(coercive, 0), -0),
    conversions,
    mixedBigInt,
  ]),
);
