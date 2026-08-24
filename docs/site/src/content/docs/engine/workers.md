---
title: "Workers"
---

Otter installs a global `Worker` constructor in every managed runtime isolate.
A worker runs on a separate managed runtime thread with its own VM, heap,
module graph, timers, async host completions, dynamic imports, and permission
state. Parent and worker exchange owned structured-clone payloads as typed
tasks over each isolate's bounded, wake-driven inbox; VM values and GC handles
never cross the boundary, and no polling channel or poll timer exists.

Constructing a `Worker` requires a managed runtime (`Otter` /
`RuntimeHandle`). A direct `Runtime` reports
`Worker requires a managed RuntimeHandle/Otter runtime` synchronously and
without any resource effect.

Every message passes a fixed pipeline: validate/measure (checked arithmetic
over nodes, keys, strings, BigInt digits, buffer bytes, and transfer
entries), admission, fallible clone, enqueue, and only then transfer
detachment — a rejected message never detaches the sender's buffers. Messages
and workers charge two ledgers atomically: the runtime's shared resource
account and a per-family hard-limit ledger (worker count, queued messages,
queued message bytes, single-message bytes) that stays finite even when the
main account is unlimited. Nested workers inherit the same family, account,
and capabilities. Each worker also reserves a guaranteed terminal credit at
construction, so its final Error/Closed outcome is delivered exactly once
even through a full parent inbox.

```js
const worker = new Worker("/absolute/path/to/worker.js");

worker.onmessage = (event) => {
  console.log("from worker", event.data);
  worker.terminate();
};

worker.onerror = (event) => {
  console.error(event.message);
  worker.terminate();
};
```

```js
// worker.js
postMessage("ready");
```

Workers inherit the parent runtime capability set as an upper bound. Current
worker options do not grant broader permissions; future narrowing options must
only remove capabilities from that inherited set.

Shared buffers are passed by shared backing storage so `Atomics` observes the
same memory from both isolates:

```js
const sab = new SharedArrayBuffer(4);
const view = new Int32Array(sab);
const worker = new Worker("/absolute/path/to/worker.js");

worker.onmessage = () => {
  console.log(Atomics.load(view, 0));
  worker.terminate();
};

worker.postMessage(sab);
```

```js
// worker.js
globalThis.onmessage = (event) => {
  const view = new Int32Array(event.data);
  Atomics.store(view, 0, 7);
  Atomics.notify(view, 0, 1);
  postMessage("stored");
};
```

`terminate()` cancels the worker's blocking `Atomics.wait` agent, interrupts
and shuts the child isolate down, and joins its thread deterministically.
User-code failures are reported as `error` events rather than panics.
