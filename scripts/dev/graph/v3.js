function run(values){ let count=0; for (let i = 0; i < 4; i++) { count = count + values[i]; } return count; }
let t=0; for (let k=0;k<20000;k++) t = run([0,1,2,3]); console.log(t);
