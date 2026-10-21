function build(n) {
  const fns = [];
  for (let i = 0; i < n; i++) {
    const add = (x) => x + i;
    fns.push(add);
  }
  let sum = 0;
  for (let k = 0; k < fns.length; k += 97) sum += fns[k](k);
  return sum;
}
let out = [];
for (let round = 0; round < 2; round++) out.push(build(2500));
console.log(out.join(","));
