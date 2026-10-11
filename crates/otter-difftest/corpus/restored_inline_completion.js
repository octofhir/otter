// A cold native construct exits a warmed inline chain. Its restored parents
// retain the suspended call source and consume the child's completion once.
function parentSites(stack, names) {
  return stack.split("\n").flatMap((line) => {
    const name = names.find((name) => line.includes(" " + name + " "));
    if (!name) return [];
    const position = line.match(/:(\d+:\d+)\)?$/)[1];
    return [name + "@" + position];
  }).join(",");
}

let successEffects = 0;
let successSites = "";
function recordSuccess(error, marker) {
  successEffects++;
  error.cause = marker;
  successSites = parentSites(error.stack, ["restoredMid", "restoredOuter"]);
  if (error.cause !== marker) throw new Error("lost success root");
  return marker.value;
}
function restoredLeaf(n, marker) {
  if (n === 7) {
    return recordSuccess(new Error("success"), marker) + n;
  }
  return n + marker.value;
}
function restoredMid(n, marker) {
  return restoredLeaf(n, marker) + 1;
}
function restoredOuter(n, marker) {
  return restoredMid(n, marker) + 1;
}
let sum = 0;
for (let i = 0; i < 20000; i++) sum += restoredOuter(i & 3, { value: 13 });
const successMarker = { value: 13 };
const success = restoredOuter(7, successMarker);
console.log("success", sum, success, successEffects, successMarker.value, successSites);

let throwEffects = 0;
function recordThrow(error, marker) {
  throwEffects++;
  error.cause = marker;
  return error;
}
function throwingLeaf(n, marker) {
  if (n === 7) {
    throw recordThrow(new Error("throw"), marker);
  }
  return n + marker.value;
}
function throwingMid(n, marker) {
  return throwingLeaf(n, marker) + marker.value;
}
function throwingOuter(n, marker) {
  return throwingMid(n, marker) + marker.value;
}
let warmThrow = 0;
for (let i = 0; i < 20000; i++) warmThrow += throwingOuter(i & 3, { value: 17 });
const throwMarker = { value: 17 };
let caught = 0;
let finished = 0;
let throwSites = "";
try {
  throwingOuter(7, throwMarker);
} catch (error) {
  caught++;
  if (error.cause !== throwMarker || error.message !== "throw") {
    throw new Error("lost thrown root");
  }
  throwSites = parentSites(error.stack, ["throwingMid", "throwingOuter"]);
} finally {
  finished++;
}
console.log("throw", warmThrow, caught, finished, throwEffects, throwMarker.value, throwSites);
