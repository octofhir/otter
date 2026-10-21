function plain(a, b) { return a * b; }
function trunc(a, b, c) { const m = a * b + c; return (m & 0x3fff) + (m >> 14); }
function sign(a, b) { const p = a * b; return 1 / p; }
function viaPhi(a, b, k) { let p = k ? a * b : a * 2; return p | 0; }
function compare(a, b) { return a * b === 0 ? 1 : 2; }
function big(a, b) { return ((a * b) >> 3) | 0; }
let acc = 0, inf = 0, neg = 0;
for (let i = 0; i < 200000; i++) {
  const a = (i % 7) - 3, b = (i % 5) - 2;
  acc = (acc + trunc(a, b, i) + viaPhi(a, b, i & 1) + compare(a, b) + big(a * 70000, b * 70000)) | 0;
  if (Object.is(plain(a, b), -0)) neg++;
  if (sign(a, b) === -Infinity) inf++;
}
console.log(acc, neg, inf, Object.is(plain(0, -1), -0), 1 / sign(0, -3));
function subChain(a, b) { return a * b - 0; }
let negSub = 0;
for (let i = 0; i < 200000; i++) if (Object.is(subChain((i % 3) - 1, i % 2 ? 0 : -1), -0)) negSub++;
console.log(negSub);
function carry(a, b, s) { return (s >> 3) + a * b; }
function carryNeg(a, b) { return a * b + (-0 | 0); }
let cz = 0, cn = 0;
for (let i = 0; i < 200000; i++) {
  if (Object.is(carry((i % 3) - 1, i % 2 ? 0 : -1, 5), -0)) cz++;
  if (Object.is(carryNeg((i % 3) - 1, i % 2 ? 0 : -1), -0)) cn++;
}
console.log(cz, cn);
