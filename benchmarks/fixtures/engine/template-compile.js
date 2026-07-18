function engineTemplateTarget(a, b) {
  let value = 0;
  if (a) {
    value = 3300;
  }
  if (!b) {
    value = 1;
  }
  return value;
}

let engineTemplateLast = 0;
for (let sample = 0; sample < 100; sample = sample + 1) {
  engineTemplateLast = engineTemplateTarget(true, true);
}
engineTemplateLast;
