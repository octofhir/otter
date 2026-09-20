// Explicit method lookup must remain committed across argument evaluation.
const methods = { abs: Math.abs, min: Math.min, max: Math.max, marker: 41 };
function explicitAbs(receiver, value) { return receiver.abs(value | 0); }
function explicitMin(receiver, left, right) { return receiver.min(left | 0, right | 0); }
function explicitMax(receiver, left, right) { return receiver.max(left | 0, right | 0); }
function explicitEffect(receiver, argument) { return receiver.abs(argument() | 0); }
function initialArgument() { return -7; }
let allocateArgument = false;
let argumentReceiver;
function argumentWithGc() {
  if (allocateArgument) {
    argumentReceiver.abs = Math.min;
    const retained = [];
    for (let index = 0; index < 128; index++) retained.push({ value: index });
    if (retained[127].value !== 127) throw new Error("argument roots lost");
  }
  return -7;
}
function explicitAllocatingArgument(receiver) { return receiver.abs(argumentWithGc() | 0); }
const allocationWarmReceiver = { marker: 50, abs: Math.abs };
for (let warm = 0; warm < 4010; warm++) {
  explicitAbs(methods, -7);
  explicitMin(methods, -7, 2);
  explicitMax(methods, -7, 2);
  explicitEffect(methods, initialArgument);
  explicitAllocatingArgument(allocationWarmReceiver);
}
const results = [
  explicitAbs(methods, -7),
  explicitMin(methods, -7, 2),
  explicitMax(methods, -7, 2),
  explicitAbs(methods, -2147483648),
];
const originalAbs = methods.abs;
let order = "";
Object.defineProperty(methods, "abs", { configurable: true, get() {
  order += "g";
  return originalAbs;
} });
results.push(explicitEffect(methods, function() { order += "a"; return -7; }), order);
Object.defineProperty(methods, "abs", { configurable: true, writable: true, value: originalAbs });
results.push(explicitEffect(methods, function() {
  methods.abs = function(value) { return this.marker + value; };
  return -7;
}));
results.push(explicitAbs(methods, -7));
order = "";
try { explicitEffect(null, function() { order += "a"; return 1; }); }
catch (error) { results.push(error instanceof TypeError, order); }
function movingReceiver(offset) {
  return { marker: 50, abs(value) { return this.marker + value + offset; } };
}
argumentReceiver = movingReceiver(5);
allocateArgument = true;
results.push(explicitAllocatingArgument(argumentReceiver));
function identity(value) { return value; }
results.push(Object.is(Math.abs(identity(-0)), 0));
results.push(Object.is(Math.min(identity(0), identity(-0)), -0));
results.push(Number.isNaN(Math.max(identity(1), identity(NaN))));
const result = JSON.stringify(results);
console.log(result);
result;
