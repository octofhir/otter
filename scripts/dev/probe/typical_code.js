"use strict";
function render(items, ctx) {
  const out = [];
  for (let i = 0; i < items.length; i++) {
    const item = items[i];
    if (!item || item.hidden) continue;
    const label = ctx.format(item.name, i);
    out.push({ id: item.id, label, width: item.width * ctx.scale });
  }
  return out.join(",");
}
render([], { format() {}, scale: 1 });
