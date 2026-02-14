// Pure counting loop - minimal overhead benchmark
let sum = 0;
for (let i = 0; i < 1000000; i++) {
  sum = sum + i;
}
console.log("result:", sum);
