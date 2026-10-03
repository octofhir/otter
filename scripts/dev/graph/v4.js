function run(n, v){ let count=0; for (let round=0; round<n; round++){ for (let i = 0; i < 4; i++) { count = count + v; } } return count; }
console.log(run(4000, 1));
