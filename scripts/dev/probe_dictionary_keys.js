// Dictionary-mode objects keyed by atom: lookups by runtime-built strings,
// keys never seen before, deletes that compact the table, integer-like and
// unicode keys, enumeration order, JSON, for-in, prototype fallbacks.
function make(n, prefix) {
  const o = {};
  for (let i = 0; i < n; i++) o[prefix + i] = i;
  return o;
}
function run() {
  const out = [];
  const d = make(300, 'k');
  out.push(Object.keys(d).length, d['k' + 7], d['k' + 299], d['nope' + Math.random()] === undefined);
  for (let i = 0; i < 300; i += 3) delete d['k' + i];
  out.push(Object.keys(d).length, d.k1, d.k3, 'k3' in d, d['k' + 298]);
  d['10'] = 'ten'; d['2'] = 'two'; d['é' + 'x'] = 'eacute'; d['\u{1f600}'] = 'smile';
  out.push(Object.keys(d).slice(0, 4).join(), d['éx'], d['\u{1f600}']);
  let count = 0, sum = 0;
  for (const k in d) { count++; if (typeof d[k] === 'number') sum += d[k]; }
  out.push(count, sum);
  const p = make(200, 'p');
  Object.setPrototypeOf(d, p);
  out.push(d.p5, d['p' + 199], 'p0' in d, Object.keys(d).length);
  out.push(JSON.stringify(make(130, 'j')).length);
  const big = Object.create(null);
  for (let i = 0; i < 5000; i++) big['w' + (i * 7919 % 5000)] = i;
  let hits = 0;
  for (let r = 0; r < 20; r++) for (let i = 0; i < 5000; i++) if (big['w' + i] !== undefined) hits++;
  out.push(hits, Object.keys(big)[0], Object.keys(big).length);
  const acc = make(150, 'a');
  Object.defineProperty(acc, 'a3', { get() { return 'got'; } });
  out.push(acc.a3, acc['a' + 3], Object.getOwnPropertyDescriptor(acc, 'a4').value);
  return out;
}
console.log(JSON.stringify(run()));
