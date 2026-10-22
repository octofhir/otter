// An observed closure context is not a permanent owner. Generated identity
// caches must follow live callees across moves and forget dead callees before
// their offsets are reused. One real closure stays live across every phase.
"use strict";
function makePayload(length, marker) {
  const payload = new Array(length).fill(marker);
  return function payloadLength() { return payload.length + payload[0]; };
}
function hotCall(callee) {
  let total = 0;
  for (let i = 0; i < 32768; i++) total += callee();
  return total;
}
function round(length, marker) {
  return hotCall(makePayload(length, marker));
}
function collect() {
  if (typeof gc === "function") { gc(); gc(); }
}
const kept = makePayload(16, 7);
let checksum = hotCall(kept);
collect();
checksum += round(8, 1);
collect();
checksum += round(262144, 3);
collect();
checksum += round(8, 5);
collect();
const replacement = function replacement() { return 31; };
checksum += hotCall(replacement);
collect();
checksum += hotCall(kept);
if (kept() !== 23 || checksum !== 8593276928) {
  throw new Error("identity recovery: " + checksum);
}
console.log("weak-identities", checksum, kept());
