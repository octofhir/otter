// String building, slicing, split/join, char scanning.
let acc = 0;
for (let r = 0; r < 200; r++) {
  let s = "";
  for (let i = 0; i < 2000; i++) s += (i % 10).toString();
  const parts = s.split("5");
  const joined = parts.join("-");
  acc += joined.length;
  let upper = 0;
  for (let i = 0; i < joined.length; i++) {
    if (joined.charCodeAt(i) >= 53) upper++;
  }
  acc += upper;
  acc += joined.slice(10, 100).indexOf("3-4");
}
console.log(acc);
