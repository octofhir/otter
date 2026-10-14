// A source keeps one context across sibling native births, then permanently widens.
function makeSpawner(seed) {
    return function spawnReader() {
        return function readCaptured(value) { return seed + value + arguments.length; };
    };
}
const spawnFirst = makeSpawner(7);
const first = spawnFirst();
let before = 0;
for (let call = 0; call < 12000; call++) before += spawnFirst()(2);
const spawnSecond = makeSpawner(11);
const second = spawnSecond();
let after = 0;
for (let call = 0; call < 12000; call++) after += first(2) + second(2);

// A collecting birth updates the cell before later sibling births compare it.
const retained = [];
for (let birth = 0; birth < 32; birth++) retained.push(spawnFirst());
let retainedTotal = 0;
for (const reader of retained) retainedTotal += reader(3);

function watchedFactory(value) {
    return {
        read: function readWatched() { return value + arguments.length; },
        write: function writeWatched(next) { value = next; },
        evalWrite: function evalWatched(next) { eval("value = next"); }
    };
}
const watched = watchedFactory(13);
let watchedTotal = 0;
for (let call = 0; call < 12000; call++) watchedTotal += watched.read();
watched.write(17);
for (let call = 0; call < 12000; call++) watchedTotal += watched.read();
watched.evalWrite(19);
for (let call = 0; call < 12000; call++) watchedTotal += watched.read();

function mappedFactory(value) {
    const args = arguments;
    return {
        read: function readMapped() { return value + arguments.length; },
        write: function writeMapped(next) { args[0] = next; }
    };
}
const mapped = mappedFactory(23);
let mappedTotal = 0;
for (let call = 0; call < 12000; call++) mappedTotal += mapped.read();
mapped.write(29);
for (let call = 0; call < 12000; call++) mappedTotal += mapped.read();

// Bound this and new.target vary independently from source context identity.
function makeArrowFactory(seed) {
    return function arrowFactory() {
        return () => [seed, this.marker, new.target === undefined];
    };
}
const arrows = makeArrowFactory(31);
const arrowA = arrows.call({marker: 3});
let arrowTotal = 0;
for (let call = 0; call < 12000; call++) arrowTotal += arrowA()[0];
const arrowB = arrows.call({marker: 4});
const constructorArrow = new arrows();

// A body spliced against the function identity also subscribes to its source fact.
function makeInline(seed) { return function inlineCaptured(value) { return seed + value; }; }
let inline = makeInline(37);
function callInline(value) { return inline(value); }
let inlineTotal = 0;
for (let call = 0; call < 12000; call++) inlineTotal += callInline(3);
inline = makeInline(41);
for (let call = 0; call < 12000; call++) inlineTotal += callInline(3);

// An OSR body relies on the same source dependency as its callable entry.
function makeLoop(seed) {
    return function capturedLoop(count) {
        let total = 0;
        for (let index = 0; index < count; index++) total += seed;
        return total;
    };
}
const loopFirst = makeLoop(43);
const loopBefore = loopFirst(60000);
const loopSecond = makeLoop(47);
const loopAfter = loopFirst(60000) + loopSecond(60000);
console.log(JSON.stringify([before, after, retainedTotal, watchedTotal, mappedTotal,
    arrowTotal, arrowA(), arrowB(), constructorArrow()[2], inlineTotal,
    loopBefore, loopAfter]));
