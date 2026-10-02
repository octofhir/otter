function thrower(x) { if (x % 1000 === 999) throw new Error("x"); return 1; }
function hot(n) {
  let acc = 0;
  for (let i = 0; i < n; i++) {
    try { acc += thrower(i); } catch (e) { acc += 100; }
  }
  return acc;
}
console.log(hot(Number(process.argv[2])));
console.log(hot(Number(process.argv[3])));
