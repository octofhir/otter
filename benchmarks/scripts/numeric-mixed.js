// Mixed numeric kernels: int32 wraparound, float accumulation, branches.
let intAcc = 0;
let floatAcc = 0;
for (let r = 0; r < 80; r++) {
  let x = r | 0;
  let y = (r * 17 + 3) | 0;
  for (let i = 0; i < 12000; i++) {
    x = (x + ((i * 1103515245 + y) | 0)) | 0;
    y = (y ^ (x >>> (i & 15))) | 0;
    if ((i & 7) === 0) floatAcc += x * 0.000001 + y * 0.0000003;
  }
  intAcc = (intAcc + x + y) | 0;
}
console.log(((intAcc >>> 0) + Math.floor(floatAcc)) % 1_000_000_007);
