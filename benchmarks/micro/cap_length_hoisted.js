function M(a){return{a:a,b:a,c:a,d:a,e:a,f:a};}
const arr=[]; for(let i=0;i<2000;i++)arr.push(M(i));
function work(arr){const n=arr.length; let s=0; for(let i=0;i<n;i++){const o=arr[i]; s+=o.a+o.b+o.c+o.d+o.e+o.f;} return s;}  // length hoisted
let s=0; for(let r=0;r<5000;r++) s+=work(arr); console.log(s);
