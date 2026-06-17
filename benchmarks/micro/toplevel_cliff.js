// Same hot property loop at MODULE TOP LEVEL — never fills the inline property
// IC at the default OSR threshold (~36M stubs, 5-9x slower than inside a fn).
// cf. cap6.js (identical loop inside work()).  Repro:
//   otter run benchmarks/micro/toplevel_cliff.js                 # slow, ~36M stubs
//   OTTER_JIT_OSR_THRESHOLD=1 otter run …/toplevel_cliff.js      # fast, inlines
function M(a){return{a:a,b:a,c:a,d:a,e:a,f:a};}
const arr=[]; for(let i=0;i<2000;i++)arr.push(M(i));
let s=0;
for(let r=0;r<3000;r++){ for(let i=0;i<2000;i++){ const o=arr[i]; s+=o.a+o.b+o.c+o.d+o.e+o.f; } }
console.log(s);
