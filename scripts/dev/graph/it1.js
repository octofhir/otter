const kinds = [1, 2, 3];
let total = 0;
for (const k of kinds) {
  for (let round = 0; round < 3000; round++) total = (total + k) | 0;
}
console.log(total);
