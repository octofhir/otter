// Fresh builtin loads: each iteration evaluates the modules in a new realm-free require cache.
const names = ["util", "path", "fs", "events", "stream", "url", "buffer", "assert"];
for (const n of names) require(n);
console.log("ok");
