console.log(typeof Worker);
const w = new Worker(new URL('./wk.js', import.meta.url).href);
w.onmessage = (e) => console.log('worker:', e.data);
w.onerror = (e) => console.log('werr', e.message);
setTimeout(() => process.exit(0), 3000);
