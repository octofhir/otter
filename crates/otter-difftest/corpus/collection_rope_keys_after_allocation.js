// Set/Map natives flatten a rope-string key before inserting it. Flattening
// allocates, so the receiver collection and the key must be read after it:
// under GC stress a stale receiver made `new Set(array)` drop the first
// occurrence of a key and insert it at its second position instead.
function ropeLog() {
  // Concatenations produce rope strings; each insertion below flattens one.
  const keys = ["read", "write", "call", "a", Symbol.unscopables, "a", "a", "b", "a",
    Symbol.unscopables, "a", "a", "b", "missingName"];
  const kinds = ["has", "has", "has", "has", "get", "has", "get", "has", "has", "get",
    "has", "set", "has", "has"];
  const log = [];
  for (let i = 0; i < keys.length; i++) {
    const churn = [];
    for (let j = 0; j < 8; j++) churn.push({ j });
    log.push(kinds[i] + ":" + String(keys[i]));
  }
  return log;
}

function run() {
  const log = ropeLog();
  const set = new Set(log);
  const map = new Map(log.map((key, index) => [key, index]));
  const probes = log.map((key) => (set.has(key) ? 1 : 0) + (map.has(key) ? 2 : 0)).join("");
  const removed = [];
  for (const key of log.slice(0, 3)) removed.push(set.delete(key), map.delete(key));
  return JSON.stringify({
    set: [...set],
    size: set.size,
    map: [...map].map(([key, value]) => key + "=" + value),
    probes,
    removed,
  });
}
run();
