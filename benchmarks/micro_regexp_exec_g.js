const re7 = /(\d*)(\D*)/g;
let n = 0;
for (let i = 0; i < 3e6; i++) { re7.lastIndex = 0; let m; while ((m = re7.exec('12ab34cd56')) && m[0]) n++; }
console.log(n);
