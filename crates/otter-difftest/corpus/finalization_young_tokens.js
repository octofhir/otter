// Young finalization targets and unregister tokens keep their identity when a
// scavenge moves them; unregister must still find every cell afterwards.
const held = [];
const tokens = [];
const refs = [];
const registry = new FinalizationRegistry(() => {});
for (let i = 0; i < 300; i++) {
  const target = { i, tag: "obj" + i };
  const token = { t: i };
  held.push(target);
  tokens.push(token);
  refs.push(new WeakRef(target));
  registry.register(target, i, token);
}
let churned = 0;
for (let r = 0; r < 20; r++) {
  const garbage = [];
  for (let j = 0; j < 8000; j++) garbage.push({ j, s: "x" + j });
  churned += garbage.length;
}
let bad = 0;
for (let i = 0; i < refs.length; i++) {
  const d = refs[i].deref();
  if (d !== held[i] || d.tag !== "obj" + i) bad++;
}
let unregistered = 0;
for (let i = 0; i < tokens.length; i++) if (registry.unregister(tokens[i])) unregistered++;
JSON.stringify({ bad, unregistered, churned });
