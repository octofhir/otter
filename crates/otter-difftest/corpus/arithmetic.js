const values = [];
for (let i = 0; i < 100; i++) values.push((2147483640 + i) * 1.5);
JSON.stringify({ overflow: values[99], nan: Number.isNaN(0 / 0), negativeZero: Object.is(-0, -0) });
