const first = await import('./update-to-dynamic-import_FIXTURE.mjs');
console.log(first.x);
const other = await first.default();
console.log(first.x, other.default);
