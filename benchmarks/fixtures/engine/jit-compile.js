function engineJitTarget(a, b) {
  let value = a;
  for (let i = 0; i < 16; i = i + 1) {
    value = value + b;
  }
  return value;
}

let engineJitSum = 0;
for (let sample = 0; sample < 100; sample = sample + 1) {
  engineJitSum = engineJitSum + engineJitTarget(1, 2);
}
engineJitSum;
