const values = [];
for (let i = 0; i < 100; i++) values.push((2147483640 + i) * 1.5);

const optimizedLeaf = () => 20 + 22;
const deoptimizedLeaf = () => 2147483647 + 1;

eval("optimizedLeaf(); deoptimizedLeaf();\n".repeat(4010));
const optimized = optimizedLeaf();
const deoptimized = deoptimizedLeaf();

JSON.stringify({
  overflow: values[99],
  nan: Number.isNaN(0 / 0),
  negativeZero: Object.is(-0, -0),
  optimized,
  deoptimized,
});
