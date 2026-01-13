console.log("event loop start");

queueMicrotask(() => {
  console.log("microtask fired");
});

Promise.resolve().then(() => {
  console.log("promise resolved");
});

setTimeout(() => {
  console.log("timeout fired");
}, 10);
