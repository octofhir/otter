// String search/scanning without split-heavy allocation dominating everything.
let text = "";
for (let i = 0; i < 2500; i++) text += "ab" + (i % 10) + "XYZ";

let acc = 0;
for (let r = 0; r < 300; r++) {
  const needle = String(r % 10) + "XY";
  let pos = 0;
  while (true) {
    const next = text.indexOf(needle, pos);
    if (next < 0) break;
    acc += next & 1023;
    pos = next + 1;
  }
  const start = (r * 17) % 5000;
  const slice = text.slice(start, start + 180);
  for (let i = 0; i < slice.length; i++) {
    const c = slice.charCodeAt(i);
    if (c >= 65 && c <= 90) acc++;
  }
}
console.log(acc);
