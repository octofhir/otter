// Compiled push/pop on numeric arrays: literal boilerplates stay intact,
// -0 / NaN / doubles round-trip, a non-number widens storage, holes and
// empty arrays read as the builtins do.
function lit() { return [1, 2.5, 3]; }
function run(n) {
  let s = 0, log = [];
  for (let i = 0; i < n; i++) {
    const a = lit();
    a.push(i, i * 0.5);
    a.push(-0);
    const z = a.pop();
    if (i === 7) log.push(Object.is(z, -0), a.length, a.join());
    a.push(NaN);
    s += Number.isNaN(a.pop()) ? 1 : 0;
    s += a.pop() + a.pop();
    if (i % 1000 === 3) { a.push('x'); log.push(a.pop(), a.length); }
    if (i % 1000 === 5) { a.push({ k: i }); log.push(a.pop().k); }
    s += a.length;
  }
  log.push(lit().join(), s);
  return log;
}
function grow(n) {
  const a = [];
  for (let i = 0; i < n; i++) a.push(i / 4);
  let t = 0;
  while (a.length) t += a.pop();
  return [t, a.pop(), a.length];
}
function holey(n) {
  const r = [];
  for (let i = 0; i < n; i++) {
    const a = [1.5, , 3.5];
    r.push(a.pop(), a.pop(), a.length);
    a.push(9);
    r.push(a.length, 1 in a);
  }
  return r.slice(-5).concat(r.length);
}
console.log(JSON.stringify(run(20000)));
console.log(JSON.stringify(grow(50000)));
console.log(JSON.stringify(holey(20000)));
