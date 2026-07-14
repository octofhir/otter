const values = [];
for (let i = 0; i < 100; i++) values.push((2147483640 + i) * 1.5);

const optimizedLeaf = () => 20 + 22;
const deoptimizedLeaf = () => 2147483647 + 1;
const optimizedIfElse = () => 3 < 8 ? 11 + 0 : 22 + 0;
const optimizedMaxPhi = () => 19 > 7 ? 19 + 0 : 7 + 0;
const optimizedAbsPhi = () => 0 > -17 ? 0 - -17 : -17 + 0;
const deoptimizedBranch = () => 7 > 3 ? 2147483647 + 1 : 0 + 0;
const optimizedLoop = (n) => {
  let sum = 0;
  for (let i = 0; i < n; i = i + 1) sum = sum + i;
  return sum;
};
const deoptimizedLoop = (n) => {
  let sum = 2147483645;
  let i = 0;
  while (i < n) {
    sum = sum + 1;
    i = i + 1;
  }
  return sum;
};
const optimizedFloatLoop = (n) => {
  let sum = 0.5;
  for (let i = 0; i < n; i = i + 1) sum = sum + 1.5;
  return sum;
};
const optimizedMixedFloat = (n) => n * 1.5 + 0.25;
const optimizedFloatDivision = (a, b) => a / b;
const optimizedFloatInfinity = (x) => x / 0;
const optimizedNanCompare = (x) => x < 0 / 0 ? 1.5 : 2.5;
const deoptimizedFloat = (x) => x + 1.5;

eval(
  "optimizedLeaf(); deoptimizedLeaf(); optimizedIfElse(); optimizedMaxPhi(); optimizedAbsPhi(); deoptimizedBranch(); optimizedLoop(16); deoptimizedLoop(1); optimizedFloatLoop(16); optimizedMixedFloat(7); optimizedFloatDivision(7.5, 2); optimizedFloatInfinity(1); optimizedNanCompare(1); deoptimizedFloat(2.5);\n".repeat(4010),
);
const optimized = optimizedLeaf();
const deoptimized = deoptimizedLeaf();
const ifElse = optimizedIfElse();
const maxPhi = optimizedMaxPhi();
const absPhi = optimizedAbsPhi();
const branchDeopt = deoptimizedBranch();
const loopSum = optimizedLoop(100);
const loopDeopt = deoptimizedLoop(5);
const floatLoopSum = optimizedFloatLoop(100);
const mixedFloat = optimizedMixedFloat(7);
const floatDivision = optimizedFloatDivision(7.5, 2);
const floatInfinity = optimizedFloatInfinity(-1);
const nanCompare = optimizedNanCompare(1);
const floatDeopt = deoptimizedFloat("otter");

JSON.stringify({
  overflow: values[99],
  nan: Number.isNaN(0 / 0),
  negativeZero: Object.is(-0, -0),
  optimized,
  deoptimized,
  ifElse,
  maxPhi,
  absPhi,
  branchDeopt,
  loopSum,
  loopDeopt,
  floatLoopSum,
  mixedFloat,
  floatDivision,
  floatInfinity,
  nanCompare,
  floatDeopt,
});
