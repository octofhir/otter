const ba = /^ba/;
let s = 0;
for (let i = 0; i < 3e7; i++) if (ba.exec('bacde')) s++;
console.log(s);
