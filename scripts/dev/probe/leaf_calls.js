function scan(src) {
  let total = 0;
  for (let i = 0; i < src.length; i++) {
    total = (total + src.charCodeAt(i)) | 0;
  }
  return total;
}
const text = "abcdefghij".repeat(100);
let sum = 0;
for (let round = 0; round < 3000; round++) sum = (sum + scan(text)) | 0;
console.log(sum);
