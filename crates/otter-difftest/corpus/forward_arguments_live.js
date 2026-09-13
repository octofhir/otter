// Forwarded arguments observe live mappings and a previously exposed object.
function list() { return Array.prototype.join.call(arguments, ','); }
function mapped(a) { a = 42; return list.apply(null, arguments); }
function duplicate(a, a) { a = 9; return list.apply(null, arguments); }
function missing(a, b) { b = 7; return list.apply(null, arguments); }
function strict(a) { 'use strict'; a = 42; return list.apply(null, arguments); }
function nonSimple(a = 0) { a = 42; return list.apply(null, arguments); }
function expose() {}
expose.apply = function(receiver, args) {
  args[0] = 42;
  args[1] = 99;
  args.length = 2;
};
function changed(a) {
  expose.apply(null, arguments);
  a = 43;
  return list.apply(null, arguments);
}
let warm = 0;
for (let i = 0; i < 5000; i++) warm += Number(mapped(i));
let result;
for (let i = 0; i < 64; i++) {
  result = [mapped(1), duplicate(1, 2, 3), missing(1), strict(1), nonSimple(1), changed(1)];
}
let lengthReads = 0;
let indexReads = 0;
let targetCalls = 0;
function prepare() {}
prepare.apply = function(receiver, args) {
  delete args[0];
  Object.defineProperty(args, 'length', {
    get() { lengthReads++; return { valueOf() { const garbage = [{}, {}]; return 2; } }; }
  });
  Object.defineProperty(args, '0', {
    get() { indexReads++; return { marker: 7 }; }
  });
  Object.defineProperty(args, '1', {
    get() { indexReads++; const garbage = [{}, {}, {}]; return { marker: 8 }; }
  });
};
function receive() { targetCalls++; return [arguments[0], arguments[1]]; }
function observed(a) { prepare.apply(null, arguments); return receive.apply(null, arguments); }
for (let i = 0; i < 64; i++) {
  const values = observed(1);
  if (values[0].marker !== 7 || values[1].marker !== 8) throw new Error('forwarded moving roots');
}
const thrownValue = { marker: 91 };
let thrown = 0;
let laterReads = 0;
function prepareThrow() {}
prepareThrow.apply = function(receiver, args) {
  args.length = 2;
  Object.defineProperty(args, '0', { get() { throw thrownValue; } });
  Object.defineProperty(args, '1', { get() { laterReads++; return 1; } });
};
function aborted(a) { prepareThrow.apply(null, arguments); return receive.apply(null, arguments); }
for (let i = 0; i < 64; i++) {
  try { aborted(1); throw new Error('missing throw'); }
  catch (error) { if (error !== thrownValue) throw error; thrown++; }
}
JSON.stringify([warm, result, lengthReads, indexReads, targetCalls, thrown, laterReads]);
