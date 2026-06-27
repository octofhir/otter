// Branch-heavy loops, switch dispatch, and boolean predicates.
let acc = 0;
for (let r = 0; r < 90; r++) {
  let state = r & 7;
  for (let i = 0; i < 18000; i++) {
    const tag = (i + state + r) & 7;
    switch (tag) {
      case 0:
      case 3:
        acc = (acc + i + state) | 0;
        state = (state + 1) & 15;
        break;
      case 1:
      case 5:
        acc = (acc ^ (i * 31)) | 0;
        state = (state ^ i) & 15;
        break;
      default:
        acc = (acc - tag + state) | 0;
        state = (state + tag) & 15;
        break;
    }
  }
}
console.log(acc >>> 0);
