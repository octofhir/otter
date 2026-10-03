let total = 0;
for (let r = 0; r < 3; r++) {
  for (const k of [1, 2, 3]) total = (total + k) | 0;
  for (let round = 0; round < 3000; round++) total = (total + 1) | 0;
}
console.log(total);
