// Timeout must stop hot loops in every tier.
function spin(n) { let s = 0; for (;;) { s = (s + n) | 0; if (s === 12345.5) break; } return s; }
function spinObj(o) { let s = 0; while (true) { s += o.k; if (s < 0) break; } return s; }
spin(1); spinObj({ k: 1 });
