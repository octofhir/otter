// Regex match / replace / exec throughput.
const text = ("The quick brown fox 123 jumps over 456 lazy dogs. " +
  "email a.b@example.com and 999-888-7777 phone. ").repeat(50);
const reWord = /\b[a-z]{4,}\b/gi;
const reNum = /\d+/g;
const reEmail = /([a-z.]+)@([a-z.]+)/gi;

let acc = 0;
for (let r = 0; r < 60; r++) {
  const words = text.match(reWord);
  acc += words ? words.length : 0;
  const replaced = text.replace(reNum, "#");
  acc += replaced.length;
  let m, c = 0;
  reEmail.lastIndex = 0;
  while ((m = reEmail.exec(text)) !== null) c++;
  acc += c;
}
console.log(acc);
