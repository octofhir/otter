let replacements = 0;
function receiverEvaluate(child, receiver) { return child; }
function receiverObserve(receiver, child) {}
function receiverBase(child) { this.child = child; this.base = 17; receiverObserve(this, child); }
function receiverMiddle(child) { receiverBase.call(this, child); this.middle = 19; }
function receiverRoot(child) { receiverMiddle.call(this, receiverEvaluate(child, this)); this.root = 23; return this; }
const warm = {marker: 731};
for (let i = 0; i < 5000; i++) new receiverRoot(warm);
// MOVING
receiverMiddle.call = function (receiver, child) { replacements++; receiver.changed = child; };
const later = {};
receiverMiddle.call(later, warm);
delete receiverMiddle.call;
for (let i = 0; i < 3; i++) new receiverRoot(warm);
// RECOVER
Object.defineProperty(receiverBase, 'call', { configurable: true, get() { replacements++; return function (r, c) { r.child = c; r.base = 41; }; } });
const result = new receiverRoot({marker: 1});
delete receiverBase.call;
console.log(JSON.stringify([result.base, result.middle, result.root, replacements]));
