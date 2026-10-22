// One source function supplies distinct prototype-held closures. A prepared
// method body belongs to its exact receiver/holder target, including context.
function capturedMethod(seed) {
    return function invokeCaptured(value) { return this.offset + seed + value; };
}
function TargetA() { this.offset = 5; }
function TargetB() { this.offset = 7; this.extra = 1; }
TargetA.prototype.invoke = capturedMethod(3);
TargetB.prototype.invoke = capturedMethod(17);
const targetA = new TargetA();
const targetB = new TargetB();

function dispatchTarget(receiver, value) { return receiver.invoke(value); }
function runTargets(rounds) {
    let total = 0;
    for (let index = 0; index < rounds; index++) {
        total += dispatchTarget((index & 1) === 0 ? targetA : targetB, index & 3);
    }
    return total;
}
const first = runTargets(32768);
if (first !== 573440) throw new Error("method target context mixed");
if (typeof gc === "function") gc();

TargetA.prototype.invoke = capturedMethod(31);
const replaced = runTargets(8192);
if (replaced !== 258048) throw new Error("method replacement used stale context");

let methodGets = 0;
const accessorMethod = capturedMethod(37);
Object.defineProperty(TargetB.prototype, "invoke", {
    configurable: true,
    get() { methodGets++; return accessorMethod; }
});
const accessor = runTargets(512);
if (accessor !== 21248 || methodGets !== 256) {
    throw new Error("method accessor effects replayed or skipped");
}

let fieldGets = 0;
Object.defineProperty(targetA, "offset", {
    configurable: true,
    get() { fieldGets++; return 11; }
});
const fieldAccessor = runTargets(512);
if (fieldAccessor !== 22784 || fieldGets !== 256 || methodGets !== 512) {
    throw new Error("receiver field descriptor proof stayed stale");
}
console.log(JSON.stringify([first, replaced, accessor, fieldAccessor, methodGets, fieldGets]));
