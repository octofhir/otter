function f(a, b, c) {
  let n = 0;
  if (a || b) n += 1;
  if (a && !b) n += 2;
  if (!(a || (b && c))) n += 4;
  while (a > 0 && (b || c)) { a--; n++; }
  do { n += 8; } while (c && n < 0);
  for (let i = 0; i < 3 && !c; i++) n += 16;
  return (a || c) ? n : -n;
}
let s = 0;
for (let i = 0; i < 200000; i++) s += f(i % 3, i % 5 === 0, i % 7 > 3);
console.log(s);
