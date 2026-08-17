'use strict';
// `node:https` — the http surface bound to TLS defaults (protocol and port
// 443). The transport below is still the plain `net` connection until a TLS
// layer exists, so a handshake with a real TLS peer does not succeed yet;
// the module shape, agents, and option handling match Node's.

const http = require('http');
const { EventEmitter } = require('events');

function Agent(options) {
  if (!(this instanceof Agent)) return new Agent(options);
  http.Agent.call(this, options);
  // The base constructor stamps own `protocol`/`defaultPort` for plain http;
  // this agent's identity is the TLS pair.
  this.protocol = 'https:';
  this.defaultPort = 443;
}
Object.setPrototypeOf(Agent.prototype, http.Agent.prototype);
Object.setPrototypeOf(Agent, http.Agent);
Agent.prototype.defaultPort = 443;
Agent.prototype.protocol = 'https:';

const globalAgent = new Agent({ keepAlive: true });

// Node routes https.request through http's ClientRequest with the https
// global agent as `_defaultAgent`; the agent supplies protocol and port 443.
function request(options, second, third) {
  if (typeof options === 'string' || options instanceof URL) {
    if (typeof second === 'object' && second !== null) {
      return http.request(options, { _defaultAgent: globalAgent, ...second }, third);
    }
    return http.request(options, { _defaultAgent: globalAgent }, second);
  }
  return http.request({ _defaultAgent: globalAgent, ...options }, second, third);
}

function get(options, second, third) {
  const client = request(options, second, third);
  client.end();
  return client;
}

function Server(options, listener) {
  if (!(this instanceof Server)) return new Server(options, listener);
  http.Server.call(this, typeof options === 'function' ? options : listener ?? options);
}
Object.setPrototypeOf(Server.prototype, http.Server.prototype);
Object.setPrototypeOf(Server, http.Server);

function createServer(options, listener) {
  return new Server(options, listener);
}

module.exports = {
  Agent,
  globalAgent,
  Server,
  createServer,
  request,
  get,
};
