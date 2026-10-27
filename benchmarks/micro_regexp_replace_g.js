const url = 'http://www.example.com:8080/path/to/page.html?query=value&x=1#frag';
const re = /\./g;
let n = 0;
for (let i = 0; i < 5e6; i++) n += url.replace(re, '_').length;
console.log(n);
