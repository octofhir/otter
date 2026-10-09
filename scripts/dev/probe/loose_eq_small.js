function scan(values) {
  let n = 0;
  for (let i = 0; i < values.length; i++)
    for (let j = 0; j < values.length; j++)
      if (values[i] == values[j]) n++;
  return n;
}
const vs = [0, 1, null, undefined, "a", {}, 1.5, true];
let t = 0;
for (let r = 0; r < 20000; r++) t += scan(vs);
console.log(t);
