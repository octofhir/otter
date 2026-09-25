// An arrow closes over the enclosing activation's `new.target`. The arrow is
// created inside a compiled constructor frame, so the compiled closure
// allocation must capture the frame's `new.target` rather than none.
function make(expected) {
  const direct = (() => new.target)();
  return [direct === expected, () => new.target];
}
let plain = 0;
let constructed = 0;
for (let i = 0; i < 600; i++) {
  const [ok, arrow] = make(undefined);
  if (ok && arrow() === undefined) plain++;
}
for (let i = 0; i < 600; i++) {
  const obj = new make(make);
  if (obj[0] && obj[1]() === make) constructed++;
}
JSON.stringify({ plain, constructed });
