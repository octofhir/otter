// parseInt across radix forms and string shapes, through a compiled call
// site whose leaf misses on strings: signs, -0, leading whitespace, hex,
// trailing garbage, long digit runs, non-Latin-1 strings, objects with
// toString/valueOf, radix coercion order, and a hot loop.
var out = [];
function pi(s, r) { return arguments.length > 1 ? parseInt(s, r) : parseInt(s); }
var cases = ['12', '-7', '+8', '-0', '0', '007', '  42', '\t9', '0x1f', '0X10', '12abc', '1e3', '', '-', '+', 'abc',
  '123456789012345', '1234567890123456', '99999999999999999999', '３', '12é', '1.9', '-0.5', 42, -3.7, null, undefined, true];
var row = [];
for (var i = 0; i < cases.length; i++) row.push(String(Object.is(pi(cases[i]), -0) ? '-0' : pi(cases[i])), String(pi(cases[i], 10)), String(pi(cases[i], 16)), String(pi(cases[i], undefined)));
out.push(row.join(','));
var order = [];
pi({ toString: function () { order.push('s'); return '11'; } }, { valueOf: function () { order.push('r'); return 2; } });
out.push('order ' + order.join(''));
out.push(String(pi({ toString: function () { return '5'; } })));
var hot = 0;
for (var k = 0; k < 50000; k++) hot += pi(['12', '345', '7', '0x1f', '-9', 8][k % 6]);
out.push('hot ' + hot);
console.log(out.join('\n'));
