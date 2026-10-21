// The retention test's subject: one long-running loop whose instanceof
// target carries Symbol.hasInstance; its optimized code is only reachable
// through OSR.
const settings = {length: 8};
let calls = 0, armed = false;
function gcSample() {}
function makePayload(length) { return {storage: new Array(length), marker: length}; }
const target = {[Symbol.hasInstance](value) {
    if (armed) gcSample(value === null ? 2 : 1);
    calls++;
    if (calls === 2 * (32768 - 1)) { settings.length = 8; armed = true; }
    return false;
}};
function retentionOnce(makePayload, target, settings, count) {
    let total = 0;
    for (let index = 0; index < count; index++) {
        try {
        let payload = makePayload(settings.length);
        payload instanceof target;
        total += payload.marker;
        payload = null;
        null instanceof target;
        } catch (error) { throw error; }
    }
    return total;
}
const warmCarrier = new Array(8);
for (let warm = 0; warm < 5000; warm++) {
    warmCarrier instanceof target;
    null instanceof target;
}
calls = 0;
console.log(JSON.stringify([retentionOnce(makePayload, target, settings, 32768), calls]));
