// Property runtime handles must survive nested scopes and abrupt reentry.
const prototype = { x: undefined };
function copyProperty(source, target) {
  target.x = source.x;
  return target.x;
}
const warm = { x: { marker: -1 } };
for (let i = 0; i < 6000; i++) copyProperty(warm, Object.create(prototype));

let gets = 0;
let sets = 0;
let caught = 0;
let checksum = 0;
for (let i = 0; i < 16; i++) {
  const payload = { marker: i, text: 'payload-' + i };
  const source = {
    get x() {
      gets++;
      const nested = copyProperty({ x: { marker: i + 100 } }, Object.create(prototype));
      if (nested.marker !== i + 100) throw new Error('nested getter root');
      if (i % 4 === 0) throw payload;
      return payload;
    }
  };
  let stored;
  const target = {
    set x(value) {
      sets++;
      const nested = copyProperty({ x: { marker: i + 200 } }, Object.create(prototype));
      if (nested.marker !== i + 200) throw new Error('nested setter root');
      stored = value;
      if (i % 5 === 0) throw value;
    },
    get x() { return stored; }
  };
  try {
    const result = copyProperty(source, target);
    if (result !== payload || result.text !== 'payload-' + i) {
      throw new Error('outer operand root');
    }
    checksum += result.marker;
  } catch (error) {
    if (error !== payload || error.marker !== i) throw error;
    caught++;
  }
  const after = copyProperty({ x: payload }, Object.create(prototype));
  if (after !== payload || after.marker !== i) throw new Error('scope cleanup');
}
JSON.stringify([gets, sets, caught, checksum]);
