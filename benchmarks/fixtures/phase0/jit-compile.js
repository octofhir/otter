function phase0JitTarget(a, b) {
  let value = a;
  for (let i = 0; i < 16; i = i + 1) {
    value = value + b;
  }
  return value;
}

let phase0JitSum = 0;
for (let sample = 0; sample < 100; sample = sample + 1) {
  phase0JitSum = phase0JitSum + phase0JitTarget(1, 2);
}
phase0JitSum;
