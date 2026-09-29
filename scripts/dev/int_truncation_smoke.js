// Word32-truncated int32 arithmetic in hot loops: `(a + b) | 0` and friends
// whose exact sums overflow int32, chains of + and -, shifts and masks of
// sums, the bitwise identities, and sums also read untruncated.
function mix(a, b, c) {
  const s1 = (a + b) | 0;
  const s2 = ((a + b) + (c - a)) | 0;
  const s3 = ((a - b) - c) >> 3;
  const s4 = ((a + 0x7fffffff) & 0xffff) ^ 0;
  const s5 = ((b + c) << 5) | 0;
  const s6 = (a + 12345) >>> 0;
  const s7 = (c - 1) & -1;
  const raw = a + b;            // read exactly: must stay checked
  const s8 = (raw | 0) + (raw > 0x7fffffff ? 1 : 0);
  return (s1 ^ s2 ^ s3 ^ s4 ^ s5 ^ (s6 | 0) ^ s7 ^ s8) | 0;
}
let h = 0;
const seeds = [0, 1, -1, 0x7fffffff, -0x80000000, 0x40000000, 123456789, -987654321, 0xffff];
for (let round = 0; round < 20000; round++) {
  for (let i = 0; i < seeds.length; i++) {
    const a = seeds[i];
    const b = seeds[(i + round) % seeds.length];
    const c = (round * 7919) | 0;
    h = (Math.imul(h, 31) + mix(a, b, c)) | 0;
  }
}
console.log(h);
