function build(n) {
  let total = 0;
  const fns = [];
  for (let i = 0; i < n; i++) {
    let local = i * 2;
    const add = (x) => x + local + total;
    fns.push(add);
    local += 1;
    total += add(1) & 7;
  }
  let sum = 0;
  for (let k = 0; k < fns.length; k += 97) sum += fns[k](k);
  return sum;
}
let out = [];
for (let round = 0; round < 2; round++) {
  out.push(build(2500));
}
console.log(out.join(","));
