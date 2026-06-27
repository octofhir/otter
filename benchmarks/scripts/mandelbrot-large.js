// Larger Mandelbrot variant to reduce startup noise in float/control-flow comparisons.
const SIZE = 420;
const ITER = 60;
let checksum = 0;
for (let py = 0; py < SIZE; py++) {
  const y0 = (py / SIZE) * 2.2 - 1.1;
  for (let px = 0; px < SIZE; px++) {
    const x0 = (px / SIZE) * 3.2 - 2.2;
    let x = 0;
    let y = 0;
    let i = 0;
    while (i < ITER && x * x + y * y <= 4.0) {
      const xx = x * x;
      const yy = y * y;
      const xy = x * y;
      x = xx - yy + x0;
      y = xy + xy + y0;
      i++;
    }
    checksum = (checksum + i + ((px ^ py) & 3)) | 0;
  }
}
console.log(checksum);
