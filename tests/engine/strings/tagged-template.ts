/* otter-test:
name = "string: tagged template receives strings + expressions"
[expect]
exit_code = 0
*/
function fail() {
  return undefined.x;
}
function tag(strings: TemplateStringsArray, ...subs: any[]): string {
  let out = "";
  for (let i = 0; i < strings.length; i = i + 1) {
    out = out + strings[i];
    if (i < subs.length) {
      out = out + "[" + subs[i] + "]";
    }
  }
  return out;
}
let name = "Otter";
let n = 7;
let r = tag`Hello, ${name}! ${n} fish.`;
if (r !== "Hello, [Otter]! [7] fish.") fail();
// `strings.raw` is attached.
function rawTag(strings: TemplateStringsArray): any {
  return strings.raw;
}
let raw = rawTag`a\nb`;
if (raw[0] !== "a\\nb") fail();
