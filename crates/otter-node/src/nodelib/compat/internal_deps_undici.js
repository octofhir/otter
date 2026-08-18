'use strict';
// Minimal surface for the vendored `http.js` lazy undici getters. The event
// classes are the web-global ones; the WebSocket client and the
// dispatcher/proxy stack are not implemented — reaching them reports that
// instead of failing the whole `http` module install when an ESM import
// enumerates the exports.

class UnimplementedWebSocket {
  constructor() {
    throw new Error('the undici WebSocket client is not implemented');
  }
}

module.exports = {
  WebSocket: globalThis.WebSocket ?? UnimplementedWebSocket,
  CloseEvent: globalThis.CloseEvent,
  MessageEvent: globalThis.MessageEvent,
  getGlobalDispatcher() {
    return undefined;
  },
  setGlobalDispatcher() {
    throw new Error('undici dispatchers are not implemented');
  },
  EnvHttpProxyAgent: class EnvHttpProxyAgent {
    constructor() {
      throw new Error('undici proxy agents are not implemented');
    }
  },
};
