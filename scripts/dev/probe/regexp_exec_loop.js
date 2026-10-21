// A long loop of one failing anchored RegExp exec, for sampling its path.
const re = /^zz/;
const s = 'hello world';
let c = 0;
for (let i = 0; i < 6e7; i++) if (re.exec(s)) c++;
console.log(c);
