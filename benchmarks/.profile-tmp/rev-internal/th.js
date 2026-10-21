process.on('uncaughtException', e => { console.log('UE', e && e.stack); });
process.on('unhandledRejection', e => { console.log('UR', e && e.stack); });
const test = require('node:test');
test('a', () => { console.log('in test'); });
setTimeout(() => { console.log('tick'); process.exit(3); }, 3000);
