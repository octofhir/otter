const trace = [];
function run(n) {
  try { trace.push("try"); if (n) throw new TypeError("boom"); return 1; }
  catch (error) { trace.push(error.name); return 2; }
  finally { trace.push("finally"); }
}
JSON.stringify({ a: run(0), b: run(1), trace });
