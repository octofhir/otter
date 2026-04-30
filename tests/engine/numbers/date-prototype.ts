/* otter-test:
name = "date: Date.prototype getters + toISOString / toJSON"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
let d = new Date(1704067200000); // 2024-01-01T00:00:00Z

// All UTC + local getters.
if (d.getTime() !== 1704067200000) fail();
if (d.valueOf() !== 1704067200000) fail();
if (d.getFullYear() !== 2024) fail();
if (d.getUTCFullYear() !== 2024) fail();
if (d.getMonth() !== 0) fail();
if (d.getUTCMonth() !== 0) fail();
if (d.getDate() !== 1) fail();
if (d.getUTCDate() !== 1) fail();
if (d.getHours() !== 0) fail();
if (d.getMinutes() !== 0) fail();
if (d.getSeconds() !== 0) fail();
if (d.getMilliseconds() !== 0) fail();
if (d.getTimezoneOffset() !== 0) fail();

// toISOString round-trip.
if (d.toISOString() !== "2024-01-01T00:00:00.000Z") fail();
if (d.toString() !== "2024-01-01T00:00:00.000Z") fail();
if (d.toJSON() !== "2024-01-01T00:00:00.000Z") fail();

// Sunday 2024-01-07 → weekday 0.
let sunday = new Date(Date.UTC(2024, 0, 7));
if (sunday.getDay() !== 0) fail();

// Invalid Date.
let invalid = new Date(NaN);
if (!Number.isNaN(invalid.getTime())) fail();
if (invalid.toJSON() !== null) fail();

// JSON.stringify routes through toJSON.
let env = JSON.stringify({ when: d });
if (env.indexOf("2024-01-01T00:00:00.000Z") < 0) fail();
