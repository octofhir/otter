const dense = [1, 2, 3];
const holey = [1, , 3];
const typed = new Int32Array(8);
for (let i = 0; i < 100; i++) { dense.push(i); typed[i & 7] += i; }

globalThis.hotValues = [1, 2.5, 3, 4.5];
function hotElement(values, index) {
  return values[index];
}
let warmSource = "";
for (let i = 0; i < 4010; i++) {
  warmSource += "hotElement(hotValues, 0);";
}
eval(warmSource);
const hotSum = hotElement(hotValues, 0) + hotElement(hotValues, 1)
  + hotElement(hotValues, 2) + hotElement(hotValues, 3);
let loadThrow = "";
try { hotElement(null, 0); } catch (error) { loadThrow = error.name; }

JSON.stringify({
  dense: dense.length,
  hole: 1 in holey,
  typed: Array.from(typed),
  hotSum,
  hotFirst: hotElement(hotValues, 0),
  loadThrow
});
