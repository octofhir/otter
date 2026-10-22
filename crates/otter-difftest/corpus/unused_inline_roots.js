// A prepared inline candidate must not retain its captured payload when the
// installed caller cannot splice its sloppy receiver binding.
function makeUnusedRootCandidate() {
  const payload = new Array(1048576).fill(1);
  return function unusedRootCandidate() {
    return payload.length + (this === globalThis ? 0 : 1);
  };
}
function unusedRootHot(cold) {
  let total = 0;
  for (let i = 0; i < 65536; i++) {
    if (i === 0) total += cold();
    total += i & 1;
  }
  return total;
}
function unusedRootRun() { return unusedRootHot(makeUnusedRootCandidate()); }
const unusedRootAnswer = unusedRootRun();
if (unusedRootAnswer !== 1081344) throw new Error("sloppy receiver binding");
if (typeof gc === "function") {
  gc(); gc();
  if (typeof process !== "undefined" && typeof process.memoryUsage === "function") {
    if (process.memoryUsage().heapUsed >= 4 * 1024 * 1024) {
      throw new Error("unspliced inline candidate retained its payload");
    }
  }
}
console.log("unused-inline-roots", unusedRootAnswer);
