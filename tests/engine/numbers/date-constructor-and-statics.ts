/* otter-test:
name = "date: Date constructor + Date.now / parse / UTC"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
// Epoch ms shape.
let zero = new Date(0);
if (zero.getTime() !== 0) fail();
if (zero.getUTCFullYear() !== 1970) fail();
if (zero.getUTCMonth() !== 0) fail();
if (zero.getUTCDate() !== 1) fail();
// Components shape: 2024 (leap) Feb 29.
let leap = new Date(2024, 1, 29, 12, 30, 45, 500);
if (leap.getUTCFullYear() !== 2024) fail();
if (leap.getUTCMonth() !== 1) fail();
if (leap.getUTCDate() !== 29) fail();
if (leap.getUTCHours() !== 12) fail();
if (leap.getUTCMinutes() !== 30) fail();
if (leap.getUTCSeconds() !== 45) fail();
if (leap.getUTCMilliseconds() !== 500) fail();
// String parsing — ISO 8601.
let parsed = new Date("2024-06-15T12:30:00Z");
if (parsed.getUTCFullYear() !== 2024) fail();
if (parsed.getUTCMonth() !== 5) fail();
if (parsed.getUTCHours() !== 12) fail();
// Statics.
let now = Date.now();
if (now < 1700000000000) fail();
if (Date.UTC(2024, 0, 1) !== 1704067200000) fail();
if (Date.parse("2024-01-01T00:00:00Z") !== 1704067200000) fail();
// Invalid string → NaN.
if (!Number.isNaN(Date.parse("not a date"))) fail();
