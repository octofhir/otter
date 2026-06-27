// Heavier regex throughput: varied patterns, global exec, replace, captures.
const chunks = [];
for (let i = 0; i < 600; i++) {
  chunks.push(
    "user" + (i % 97) + ".team" + (i % 13) + "@example" + (i % 7) + ".com " +
      "id=" + (100000 + i * 37) + " path=/api/v" + (i % 4) + "/items/" + i + " " +
      (i % 5 === 0 ? "WARN" : "INFO") + " latency=" + ((i * 17) % 1000) + "ms\n",
  );
}
const text = chunks.join("");

const emails = /([a-z0-9.]+)@([a-z0-9.]+)\.com/g;
const ids = /\bid=(\d{5,})\b/g;
const paths = /\/api\/v([0-9])\/items\/([0-9]+)/g;
const levels = /\b(INFO|WARN)\b/g;

let acc = 0;
for (let r = 0; r < 70; r++) {
  let m;
  emails.lastIndex = 0;
  while ((m = emails.exec(text)) !== null) acc += m[1].length + m[2].length;
  ids.lastIndex = 0;
  while ((m = ids.exec(text)) !== null) acc += m[1].charCodeAt(0);
  paths.lastIndex = 0;
  while ((m = paths.exec(text)) !== null) acc += (+m[1]) + (+m[2] & 31);
  const replaced = text.replace(levels, r & 1 ? "DBG" : "TRACE");
  acc += replaced.length & 65535;
}
console.log(acc);
