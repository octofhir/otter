const values = [0, -0, 1, 1.5, "", "ab", null, undefined, true, {}, NaN];
function eq(a, b) { return a === b; }
function run() {
  let count = 0;
  for (let round = 0; round < 400; round++) {
    for (let i = 0; i < values.length; i++) {
      for (let j = 0; j < values.length; j++) {
        if (eq(values[i], values[j])) count++;
      }
    }
  }
  return count;
}
console.log(run());
