const re13 = /\?[\w\W]*(sevraqvq|punaaryvq|tebhcvq)=([^\&\?#]*)/i;
let n = 0;
for (let i = 0; i < 5e6; i++) { if (re13.exec('?a=1&punaaryvq=42&b')) n++; }
console.log(n);
