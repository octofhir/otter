function hot(n) {
  let checksum = 0;
  let last = "";
  const words = ["ab", "abc", "abcd"];
  for (let i = 0; i < n; i++) {
    last = words[i % 3];
    checksum = (checksum + last.length + (i & 7)) | 0;
  }
  return checksum + "|" + last;
}
console.log(hot(3000));
