function counter(start) { let n = start; return () => ++n; }
const next = counter(40);
let trace = [];
for (let i = 0; i < 100; i++) trace.push(next());
JSON.stringify({ first: trace[0], last: trace[99] });
