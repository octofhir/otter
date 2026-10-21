const re = /(\w+)@(\w+)\.(\w+)/;
const subjects = [];
for (let i = 0; i < 64; i++) subjects.push("user" + i + "@host" + (i * 7) + ".org trailing text");
let sum = 0;
for (let k = 0; k < 40000; k++) {
  for (let i = 0; i < 64; i++) {
    const m = re.exec(subjects[i]);
    sum += m.index + m[1].length + m[3].length;
  }
}
console.log(sum);
