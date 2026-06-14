// Mandelbrot set escape-time — tight float inner loop + branch.
const SIZE = 256;
const ITER = 40;
let checksum = 0;
for (let py = 0; py < SIZE; py++) {
  const y0 = (py / SIZE) * 2.0 - 1.0;
  for (let px = 0; px < SIZE; px++) {
    const x0 = (px / SIZE) * 3.0 - 2.0;
    let x = 0, y = 0, i = 0;
    while (i < ITER && x * x + y * y <= 4.0) {
      const xt = x * x - y * y + x0;
      y = 2.0 * x * y + y0;
      x = xt;
      i++;
    }
    checksum += i;
  }
}
console.log(checksum);
