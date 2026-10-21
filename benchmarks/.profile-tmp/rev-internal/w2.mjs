import { Worker } from 'node:worker_threads';
const w = new Worker(new URL('data:text/javascript,import("node:internal/util").then(()=>console.log("LEAK"),e=>console.log("wk",e.code))'));
w.on('error', e => console.log('werr', e.message));
