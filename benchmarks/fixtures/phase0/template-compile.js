function phase0TemplateTarget(a, b) {
  let value = 0;
  if (a) {
    value = 3300;
  }
  if (!b) {
    value = 1;
  }
  return value;
}

let phase0TemplateLast = 0;
for (let sample = 0; sample < 100; sample = sample + 1) {
  phase0TemplateLast = phase0TemplateTarget(true, true);
}
phase0TemplateLast;
