// OSR enters an inner loop while values flow through sibling and outer loops.
function run(n) {
  let a = 1, b = 2, c = 3, d = 0, total = 0;
  for (let k = 0; k < 3; k++) {           // sibling loop before the hot one
    a += k; d ^= a;
  }
  for (let outer = 0; outer < 4; outer++) {
    let s = b;
    for (let i = 0; i < n; i++) {         // hot loop: OSR target
      s = (s + i * c) | 0;
      if ((i & 1023) === 0) b ^= i;
    }
    for (let j = 0; j < 5; j++) {         // sibling loop after, inside outer
      total = (total + s + a + j) | 0;
    }
    c = (c * 3 + outer) | 0;
  }
  let w = 0;
  while (w < 7) { total = (total ^ (d + w)) | 0; w++; }   // loop after everything
  return [a, b, c, d, total];
}
console.log(run(300000).join(","));
console.log(run(10).join(","));
