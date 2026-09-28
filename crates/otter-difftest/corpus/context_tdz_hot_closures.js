// A closure that reads a captured `let` before its declaration runs throws
// ReferenceError in every tier, also after the declaring function or loop has
// tiered up.
function kindOf(f) {
  try { return "ok:" + f(); } catch (e) { return e.constructor.name; }
}
function tally(kinds, k) { kinds[k] = (kinds[k] || 0) + 1; }

function hotBlockTdz(n) {
  const kinds = {};
  let sum = 0;
  for (let i = 0; i < n; i++) {
    {
      const read = () => cell.n;
      if (i % 97 === 0) tally(kinds, kindOf(read));
      let cell = { n: i };
      sum += read();
    }
  }
  return sum + " " + JSON.stringify(kinds);
}

function hotFunctionTdz(n) {
  const kinds = {};
  function make(i) {
    const read = () => cell;
    if (i % 97 === 0) tally(kinds, kindOf(read));
    let cell = { n: i };
    return read().n;
  }
  let sum = 0;
  for (let i = 0; i < n; i++) sum += make(i);
  return sum + " " + JSON.stringify(kinds);
}

function run() {
  const lines = [];
  lines.push("sloppy=" + (function () { return this !== undefined; })());
  lines.push("hot-block " + hotBlockTdz(3000));
  lines.push("hot-function " + hotFunctionTdz(3000));
  for (const line of lines) console.log(line);
  return "context_tdz_closures_pending:" + lines.length;
}
run();
