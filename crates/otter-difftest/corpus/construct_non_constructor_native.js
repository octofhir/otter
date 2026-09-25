// `new` of a callable native without [[Construct]] throws TypeError in every
// tier. The construct sits in a small closure invoked through a helper, so the
// optimizing tier compiles it as a generic construct that saw many callees.
function outcome(f) {
  try {
    f();
  } catch (e) {
    return e instanceof TypeError ? "threw" : "other";
  }
  return "constructed";
}
function attempt(method) {
  return outcome(function () { new method(); });
}
const natives = [];
for (const holder of [Object, Math, JSON, Array.prototype, String.prototype, Number.prototype]) {
  for (const name of Object.getOwnPropertyNames(holder)) {
    if (name === "constructor" || name === "fromAsync") continue;
    const value = holder[name];
    if (typeof value === "function") natives.push(value);
  }
}
const counts = { constructed: 0, threw: 0, other: 0 };
for (let round = 0; round < 30; round++) for (const f of natives) counts[attempt(f)]++;
JSON.stringify(counts);
