const values = [0, 1, 2, 3];
function run() {
  let count = 0;
  for (let round = 0; round < 4000; round++) {
    for (let i = 0; i < values.length; i++) {
      count = count + values[i];
    }
  }
  return count;
}
console.log(run());
