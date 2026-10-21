require.cache['internal/string_decoder_binding'] = { exports: { POISON: 1 }, loaded: true };
try { const { StringDecoder } = require('string_decoder'); console.log(new StringDecoder('utf8').write(Buffer.from('hi'))); } catch (e) { console.log('builtin used planted module:', e.message); }
