// D4: Math.sqrt routed through call bridge -> libm, not native fsqrt.
// otter ~1748ms vs node ~5ms compute (~100x+) for 10M sqrt.
function work(){let s=0; for(let i=1;i<5000000;i++){ s+=Math.sqrt(i)*Math.sqrt(i+1); } return s;}
console.log(work().toFixed(2));