
// Verify Date static methods non-constructability
function assertThrowsTypeError(fn, name) {
    try {
        new fn();
        console.log(`FAIL: new ${name}() should throw TypeError`);
    } catch (e) {
        if (e instanceof TypeError) {
            console.log(`PASS: new ${name}() throws TypeError`);
        } else {
            console.log(`FAIL: new ${name}() threw ${e}, expected TypeError`);
        }
    }
}

assertThrowsTypeError(Date.parse, "Date.parse");
assertThrowsTypeError(Date.now, "Date.now");
assertThrowsTypeError(Date.UTC, "Date.UTC");

// Date.parse should return NaN for undefined/empty/invalid
if (Number.isNaN(Date.parse())) console.log("PASS: Date.parse() -> NaN");
else console.log(`FAIL: Date.parse() -> ${Date.parse()}`);

if (Number.isNaN(Date.parse("invalid"))) console.log("PASS: Date.parse('invalid') -> NaN");
else console.log(`FAIL: Date.parse('invalid') -> ${Date.parse("invalid")}`);

// Date.UTC
const utc = Date.UTC(2020, 0, 1);
if (utc === 1577836800000) console.log("PASS: Date.UTC(2020, 0, 1) correct");
else console.log(`FAIL: Date.UTC(2020, 0, 1) = ${utc}`);

// Date.parse formats
const cases = [
    ["2020", 1577836800000],
    ["2020-01", 1577836800000],
    ["2020-01-01", 1577836800000],
    ["2020-01-01T00:00:00.000Z", 1577836800000],
    // Local time check (TZ=UTC enforced)
    ["2020-01-01T00:00:00", 1577836800000],
    ["2020-01-01T00:00", 1577836800000] // Partial time
];

for (let i = 0; i < cases.length; i++) {
    const str = cases[i][0];
    const expected = cases[i][1];
    const actual = Date.parse(str);
    if (actual === expected) {
        console.log(`PASS: Date.parse("${str}") = ${actual}`);
    } else {
        console.log(`FAIL: Date.parse("${str}") = ${actual}, expected ${expected}`);
    }
}

// Prototype checks
if (Date.prototype.constructor === Date) console.log("PASS: Date.prototype.constructor === Date");
else console.log("FAIL: Date.prototype.constructor !== Date");

if (Object.prototype.toString.call(new Date()) === "[object Date]") console.log("PASS: [object Date] correct");
else console.log(`FAIL: toString = ${Object.prototype.toString.call(new Date())}`);

// toJSON
const d = new Date("2020-01-01T00:00:00.000Z");
if (d.toJSON() === "2020-01-01T00:00:00.000Z") console.log("PASS: toJSON correct");
else console.log(`FAIL: toJSON = ${d.toJSON()}`);
