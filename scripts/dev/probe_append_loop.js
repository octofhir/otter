// A long run of `a[i] = i` appends into fresh arrays.
let total = 0;
for (let r = 0; r < 400000; r++) {
  const a = [];
  for (let i = 0; i < 50; i++) a[i] = i;
  total += a.length;
}
console.log(total);
