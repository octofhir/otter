# Workers

Otter installs a global `Worker` constructor in every runtime isolate. A worker
runs on a separate runtime thread with its own VM, heap, module graph, timers,
and permission state. Parent and worker exchange owned structured-clone payloads
over host channels; VM values and GC handles never cross the boundary.

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

`terminate()` closes the worker input channel, interrupts the runtime, and wakes
blocking `Atomics.wait` waiters. User-code failures are reported as `error`
events rather than panics.
