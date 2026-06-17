function M(a){return{a:a,b:a,c:a,d:a,e:a,f:a};}        // 6 props: all inline (slot 0-5)
const arr=[]; for(let i=0;i<2000;i++)arr.push(M(i));
function work(arr){let s=0; for(let i=0;i<arr.length;i++){const o=arr[i]; s+=o.a+o.b+o.c+o.d+o.e+o.f;} return s;}
let s=0; for(let r=0;r<5000;r++) s+=work(arr); console.log(s);
