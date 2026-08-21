'use strict';

// The host side of Node's timer machinery.
//
// Node keeps its own timer lists in JavaScript and asks the loop for exactly
// one timer at a time: the next expiry. When that timer fires the loop calls
// back in, and the list decides which callbacks are due. `setImmediate` is the
// same arrangement one phase later — a queue in JavaScript, drained when the
// loop reaches its check phase.
//
// The engine's own timers are what stands in for the loop here. They are
// captured before Node's timers replace the globals, so the two never chase
// each other, and only one of them is ever armed.

// The engine's own timers.
//
// They cannot simply be read off `globalThis`: by the time this module loads,
// those names may answer from the accessor that loads it, and reading one
// would ask for this module again. The layer that installs that accessor
// leaves them on the accessor itself, which can be looked at without being
// called. A realm that never replaced the globals still has them there.
//
// Resolved now, as this module loads. The layer that replaces the globals
// loads it on its way to replacing them, so at this moment those names still
// mean the engine's timers — a moment later they would mean Node's, and the
// loop would be driving itself.
const engine = (() => {
  const named = Object.getOwnPropertyDescriptor(globalThis, 'setTimeout');
  return named?.get?.engineTimers ?? {
    setTimeout: globalThis.setTimeout,
    clearTimeout: globalThis.clearTimeout,
    setImmediate: globalThis.setImmediate,
    clearImmediate: globalThis.clearImmediate,
    setRef: globalThis.__otterTimerSetRef,
  };
})();
function eng() {
  return engine;
}

// `kCount`, `kRefCount`, `kHasOutstanding` — the fields Node's list reads and
// writes to say how many immediates are queued, how many of them hold the loop
// open, and whether a drain left work behind.
const immediateInfo = new Uint32Array(3);
// The number of timers that hold the loop open.
const timeoutInfo = new Int32Array(1);

let processImmediate = null;
let processTimers = null;

// The one engine timer standing for the next expiry, and whether it holds the
// loop open.
let armed = null;
let armedRefed = true;

// The one engine immediate standing for the check phase.
let checkArmed = null;
let checkRefed = true;

// A monotonic clock in whole milliseconds. Node's lists only ever compare and
// subtract these, so what matters is that it never goes backwards — which the
// wall clock does not promise.
const origin = process.hrtime.bigint();
function getLibuvNow() {
  return Number((process.hrtime.bigint() - origin) / 1000000n);
}

// Arm the one timer standing for the next expiry, `msecs` from now.
//
// What the list answers when it runs is the next expiry after the ones it just
// ran: zero for none left, and otherwise the moment itself, negative when no
// timer left holds the loop open. Re-arming from that answer is what keeps a
// second round of timers — an interval, a refresh — coming.
function scheduleTimer(msecs) {
  if (armed !== null) eng().clearTimeout(armed);
  armed = eng().setTimeout(fire, msecs);
  if (!armedRefed) eng().setRef(Number(armed), false);
}

function fire() {
  armed = null;
  const expiry = processTimers(getLibuvNow());
  if (expiry === 0) return;
  armedRefed = expiry > 0;
  const remaining = Math.abs(expiry) - getLibuvNow();
  scheduleTimer(remaining > 0 ? remaining : 1);
}

function toggleTimerRef(refed) {
  armedRefed = refed;
  if (armed !== null) eng().setRef(Number(armed), refed);
}

// Reaching the check phase: drain what is queued, and come back if the drain
// left more behind, which is what an immediate scheduling an immediate does.
function runCheckPhase() {
  checkArmed = null;
  processImmediate();
  armCheckPhase();
}

function armCheckPhase() {
  if (immediateInfo[0] === 0 || checkArmed !== null) return;
  checkArmed = eng().setImmediate(runCheckPhase);
  if (!checkRefed) eng().setRef(Number(checkArmed), false);
}

function toggleImmediateRef(refed) {
  checkRefed = refed;
  if (checkArmed !== null) eng().setRef(Number(checkArmed), refed);
}

function setupTimers(immediateCallback, timersCallback) {
  processImmediate = immediateCallback;
  processTimers = timersCallback;
}

module.exports = {
  immediateInfo,
  timeoutInfo,
  getLibuvNow,
  scheduleTimer,
  toggleTimerRef,
  toggleImmediateRef,
  setupTimers,
  // Node's list queues an immediate by bumping `immediateInfo`; the loop is
  // what notices. Nothing tells us, so the timers module says so itself.
  armCheckPhase,
};
