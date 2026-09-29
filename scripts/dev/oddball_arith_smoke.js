// Generic binary operators over undefined / null / Booleans mixed with
// Numbers in a hot loop: ToNumber of each immediate, every operator family.
const values = [undefined, null, true, false, 0, -0, 1, -1, 2.5, -7.25, 0x7fffffff, -0x80000000, 4294967295, NaN];
function mix(a, b) {
  return [a + b, a - b, a * b, a / b, a & b, a | b, a ^ b, a << b, a >> b, a >>> b, a < b, a <= b, a > b, a >= b];
}
let acc = 0;
let text = '';
for (let round = 0; round < 400; round++) {
  for (const a of values) {
    for (const b of values) {
      const r = mix(a, b);
      for (const v of r) acc = (acc + (typeof v === 'number' ? (Number.isNaN(v) ? 7 : v) : (v ? 3 : 5))) % 1000003;
      if (round === 399) text += r.map((v) => Object.is(v, -0) ? '-0' : String(v)).join(',') + ';';
    }
  }
}
let h = 0;
for (let i = 0; i < text.length; i++) h = (h * 31 + text.charCodeAt(i)) | 0;
console.log(acc, h, text.length);
