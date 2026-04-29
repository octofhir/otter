/* otter-test:
name = "temporal: Temporal.Instant.from + epochMilliseconds"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const inst = Temporal.Instant.from("2024-01-01T00:00:00Z");
if (inst.epochMilliseconds !== 1704067200000) fail();
const inst2 = Temporal.Instant.fromEpochMilliseconds(1704067200000);
if (inst2.epochMilliseconds !== 1704067200000) fail();
if (inst.equals(inst2) !== true) fail();
