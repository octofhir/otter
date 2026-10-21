function fresh(n) {
  let distinct = 0;
  let prev = null;
  for (let i = 0; i < n; i++) {
    const re = /a+b/g;
    if (re !== prev) distinct++;
    prev = re;
  }
  return distinct;
}
console.log(fresh(3000), fresh(3000));
