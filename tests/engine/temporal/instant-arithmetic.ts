/* otter-test:
name = "temporal: Instant.add / subtract with Duration partials"
[expect]
exit_code = 0
*/
function fail() {
    return undefined.x;
}
const start = Temporal.Instant.from("2024-01-01T00:00:00Z");
const later = start.add({ hours: 1 });
if (later.epochMilliseconds !== 1704067200000 + 3600000) fail();
const back = later.subtract({ hours: 1 });
if (back.epochMilliseconds !== start.epochMilliseconds) fail();
const cmp = Temporal.Instant.compare(start, later);
if (cmp !== -1) fail();
